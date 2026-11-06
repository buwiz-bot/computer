//! Computer Use (CUA) actuators against the box X display.
//!
//! Input is in-process XTEST on a persistent X connection (xdotool is a
//! fallback). Screenshots use GetImage + fast PNG (import/scrot fallback).
//! This crate never calls an inference gateway — models live in L4.

mod cook_record;
mod encode;
mod keys;
mod observe;
mod recipe;
mod settle;
mod voxl;
pub(crate) mod x11;
mod xdotool;

use std::env;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use arrayvec::ArrayVec;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use box_common::env_bool;
use box_desktop::{parse_geometry, DISPLAY_HEIGHT, DISPLAY_WIDTH};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use tokio::sync::Mutex;

pub use encode::{bgra_to_rgb, bgra_to_rgb_unchecked, encode_png_rgb, zpixmap_to_rgb};
pub use keys::{char_to_keysym, parse_key_sequence, KeySeq};
pub use observe::{Focus, FocusState, RecipeObserve, StepObservation, WindowRef};
pub use recipe::{
    run_recipe, validate_recipe, RecipeArtifact, RecipeRequest, RecipeResponse, RecipeScreenshot,
    RecipeSettle, RecipeStep, RecipeStepResult, MAX_RECIPE_STEPS, MAX_WAIT_MS,
};
pub use voxl::{RecordStarted, RecordStopRequest, ViewerClient, WaitRequest, WaitResponse};

static POINTER_WARMED: AtomicBool = AtomicBool::new(false);
static ACTUATOR: Mutex<()> = Mutex::const_new(());

/// Gap between mousedown and mouseup for a synthetic click. `xdotool click`
/// sleeps 100ms; a few milliseconds is enough for tint2/Openbox.
const CLICK_GAP: Duration = Duration::from_millis(12);
/// Openbox starts a move grab after ButtonPress; a same-invocation warp is
/// often dropped. Live press→release from L1 already has a human-scale gap.
const DRAG_GRAB: Duration = Duration::from_millis(20);
/// How long a verb waits for the actuator before giving up on it.
///
/// `ACTUATOR` protects a *gesture*, not a single X request: press, gap,
/// release is one click, and another verb's motion spliced into the middle
/// of it is not a click. That sequence genuinely cannot be interrupted —
/// cancelling it half-way would leave a mouse button held down — so the
/// lock stays held across the whole gesture and the bound comes from the
/// per-call `XTEST_DEADLINE` instead: every step now fails in bounded time,
/// so the hold is bounded too. The worst of them is `double_click`, three
/// XTEST deadlines plus its two gaps, about 6.1s. Ten seconds clears that
/// and still turns a stuck desk into a 503 rather than a queue that never
/// drains. A bulk `type` of thousands of characters can outlast it; a
/// caller racing one gets 503 and can retry.
const ACTUATOR_WAIT: Duration = Duration::from_secs(10);
pub(crate) const MAX_MOTION_PATH: usize = 64;
/// `drag_waypoints` plus a release path plus one extra coordinate.
pub(crate) const MAX_COLLECTED_POINTS: usize = MAX_MOTION_PATH + MAX_DRAG_WAYPOINTS;
/// `drag_waypoints` emits `steps+1` points with `steps` clamped to 8..=24.
pub(crate) const MAX_DRAG_WAYPOINTS: usize = 25;

/// Warp the pointer once without `--sync`.
///
/// xdotool 3.20160805 `xdo_wait_for_mouse_move_from` uses `MAX_TRIES` 500
/// and `usleep(30000)`: **15s when the warp target is already the pointer**.
/// Xvfb starts the pointer at the display center (640,400 on 1280×800).
/// Screenshot does not move it, so the first `mousemove --sync 640 400`
/// (L1 default click / scroll recipe) waits the full 15s. A non-sync warp
/// to (16,16) leaves center so a leftover `--sync` guest returns immediately.
pub async fn warmup_pointer(config: &CuaConfig) {
    if !config.enabled || POINTER_WARMED.load(Ordering::Relaxed) {
        return;
    }
    for attempt in 0..25 {
        if !display_up(&config.display) {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }
        let started = Instant::now();
        let Ok(_g) = lock_actuator(config).await else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        // Off-center: Xvfb's default pointer is (width/2, height/2), not (0,0).
        match x11::move_pointer(config, 16, 16).await {
            Ok(()) => {
                tracing::info!(
                    attempt,
                    ms = started.elapsed().as_millis() as u64,
                    "cua pointer warmup ok"
                );
                POINTER_WARMED.store(true, Ordering::Relaxed);
                return;
            }
            Err(err) => {
                tracing::warn!(attempt, error = %err, "cua pointer warmup retry");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
    tracing::warn!("cua pointer warmup gave up; first click may be slow");
}

/// Take the actuator, or report that the desk is stuck.
///
/// `DisplayDown` maps to 503 in `box-exec`. A caller that waited this long
/// behind a gesture is not looking at a request that failed; it is looking
/// at a display that is not usable right now.
async fn lock_actuator(
    config: &CuaConfig,
) -> Result<tokio::sync::MutexGuard<'static, ()>, CuaError> {
    match tokio::time::timeout(ACTUATOR_WAIT, ACTUATOR.lock()).await {
        Ok(guard) => Ok(guard),
        Err(_) => {
            tracing::error!(
                display = %config.display,
                ms = ACTUATOR_WAIT.as_millis() as u64,
                "cua actuator is still held; refusing to queue"
            );
            Err(CuaError::DisplayDown(format!(
                "{} (actuator held for more than {}ms)",
                config.display,
                ACTUATOR_WAIT.as_millis()
            )))
        }
    }
}

/// Capability flag name advertised by `box-host`.
pub const CAPABILITY: &str = "cua";

/// Display geometry for CUA actuators.
///
/// Field order packs the `String` then the integers then the flag so there
/// is no alignment hole between `enabled: bool` and `display`. Not `repr(C)`
/// (no FFI) and not `packed` (that would unalign the `u32`s).
#[derive(Clone, Debug)]
pub struct CuaConfig {
    pub display: String,
    pub width: u32,
    pub height: u32,
    pub enabled: bool,
}

impl CuaConfig {
    pub fn from_env() -> Self {
        let geom = env::var("BOX_DISPLAY_GEOM").unwrap_or_else(|_| "1280x800x24".into());
        let (width, height) = parse_geometry(&geom).unwrap_or((DISPLAY_WIDTH, DISPLAY_HEIGHT));
        Self {
            display: env::var("BOX_DISPLAY").unwrap_or_else(|_| ":1".into()),
            width,
            height,
            enabled: env_bool("BOX_CUA", true),
        }
    }

    pub fn disabled() -> Self {
        Self {
            display: ":1".into(),
            width: DISPLAY_WIDTH,
            height: DISPLAY_HEIGHT,
            enabled: false,
        }
    }

    /// This config on another of the box's screens: a Bot's own screen on a computer several
    /// Bots share is its own X display (`box-screen`), with the same size as the shared one.
    /// `None` is the box's own display. Only a display name (`:N`) is taken.
    pub fn on(&self, display: Option<&str>) -> Result<Self, CuaError> {
        let display = display
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .unwrap_or(&self.display);
        let number = display.strip_prefix(':').unwrap_or("");
        let named =
            !number.is_empty() && number.len() <= 3 && number.bytes().all(|b| b.is_ascii_digit());
        if !named {
            return Err(CuaError::Invalid(format!(
                "display must be :N, not {display:?}"
            )));
        }
        // A client gives its cookie to whatever holds the name, and any process can take the name
        // of a screen that's down. Nothing connects until that's the screen's own server.
        if self.enabled && verify_servers() && !box_desktop::server_is_the_screens(display) {
            return Err(CuaError::DisplayDown(display.to_string()));
        }
        Ok(Self {
            display: display.to_string(),
            ..self.clone()
        })
    }

    /// True when CUA is enabled and the X display socket is present.
    pub fn capability_ready(&self) -> bool {
        self.enabled && display_up(&self.display)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CuaError {
    #[error("computer use is disabled")]
    Disabled,
    #[error("display {0} is not available")]
    DisplayDown(String),
    #[error("coordinates ({0},{1}) are outside {2}x{3}")]
    OutOfRange(i32, i32, u32, u32),
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("tool failed: {0}")]
    Tool(String),
    #[error("viewer unavailable: {0}")]
    Viewer(String),
}

#[derive(Debug, Deserialize)]
pub struct ClickRequest {
    pub x: i32,
    pub y: i32,
    pub button: Option<u8>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyAction {
    #[default]
    Tap,
    Down,
    Up,
}

#[derive(Debug, Deserialize)]
pub struct TypeRequest {
    pub text: String,
}

#[derive(Debug, Deserialize)]
pub struct KeyRequest {
    pub key: String,
    #[serde(default)]
    pub action: Option<KeyAction>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct CuaPoint {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Deserialize)]
pub struct ReleaseRequest {
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub button: Option<u8>,
    #[serde(default)]
    pub path: Option<Vec<CuaPoint>>,
}

#[derive(Debug, Deserialize)]
pub struct ScrollRequest {
    pub x: i32,
    pub y: i32,
    pub dx: i32,
    pub dy: i32,
}

#[derive(Debug, Deserialize)]
pub struct DragRequest {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
    pub button: Option<u8>,
}

#[derive(Debug, Deserialize)]
pub struct MoveRequest {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScreenshotResponse {
    pub encoding: &'static str,
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub png_base64: String,
    pub bytes: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DisplayInfo {
    pub display: String,
    pub width: u32,
    pub height: u32,
    pub available: bool,
}

#[derive(Debug, Serialize)]
pub struct OkResponse {
    pub ok: bool,
}

pub fn display_info(config: &CuaConfig) -> DisplayInfo {
    DisplayInfo {
        display: config.display.clone(),
        width: config.width,
        height: config.height,
        available: config.enabled && display_up(&config.display),
    }
}

pub fn validate_point(config: &CuaConfig, x: i32, y: i32) -> Result<(), CuaError> {
    if x < 0 || y < 0 || x >= config.width as i32 || y >= config.height as i32 {
        return Err(CuaError::OutOfRange(x, y, config.width, config.height));
    }
    Ok(())
}

pub async fn screenshot_png(config: &CuaConfig) -> Result<Vec<u8>, CuaError> {
    ensure_ready(config)?;
    let display = config.display.clone();
    let width = config.width;
    let height = config.height;
    let native = tokio::task::spawn_blocking(move || x11::capture_png(&display, width, height))
        .await
        .map_err(|err| CuaError::Tool(format!("screenshot worker: {err}")))?;
    match native {
        Ok(png) => check_png(png),
        Err(err) => {
            tracing::debug!(error = %err, "native GetImage failed; trying import/scrot");
            let png = xdotool::capture_png(&config.display).await?;
            check_png(png)
        }
    }
}

fn check_png(png: Vec<u8>) -> Result<Vec<u8>, CuaError> {
    if png.len() < 8 {
        return Err(CuaError::Tool("screenshot was empty".into()));
    }
    // SAFETY: `len >= 8` from the check above.
    let sig = unsafe { png.get_unchecked(..8) };
    if sig != b"\x89PNG\r\n\x1a\n" {
        return Err(CuaError::Tool("screenshot was not a PNG".into()));
    }
    Ok(png)
}

pub fn screenshot_from_png(
    config: &CuaConfig,
    png: Vec<u8>,
    path: Option<String>,
    inline: bool,
) -> ScreenshotResponse {
    let bytes = png.len();
    let png_base64 = if inline {
        BASE64.encode(&png)
    } else {
        String::new()
    };
    ScreenshotResponse {
        encoding: if path.is_some() && !inline {
            "file"
        } else {
            "base64"
        },
        mime: "image/png",
        width: config.width,
        height: config.height,
        bytes,
        png_base64,
        path,
    }
}

pub async fn screenshot(config: &CuaConfig) -> Result<ScreenshotResponse, CuaError> {
    let png = screenshot_png(config).await?;
    Ok(screenshot_from_png(config, png, None, true))
}

fn validate_button(button: Option<u8>) -> Result<u8, CuaError> {
    let button = button.unwrap_or(1);
    if !(1..=7).contains(&button) {
        return Err(CuaError::Invalid("button must be 1-7".into()));
    }
    Ok(button)
}

fn validate_key(key: &str) -> Result<(), CuaError> {
    if key.is_empty() {
        return Err(CuaError::Invalid("key must not be empty".into()));
    }
    if key.chars().any(|c| c.is_whitespace() || c == ';') {
        return Err(CuaError::Invalid("key contains invalid characters".into()));
    }
    Ok(())
}

/// Never `mousemove --sync` (same-spot XQueryPointer poll = 15s). Never
/// `xdotool click` (cmd_click sleeps 100ms after the press).
pub async fn click(config: &CuaConfig, req: &ClickRequest) -> Result<OkResponse, CuaError> {
    ensure_ready(config)?;
    validate_point(config, req.x, req.y)?;
    let button = validate_button(req.button)?;
    let _g = lock_actuator(config).await?;
    x11::pointer_press(config, req.x, req.y, button).await?;
    tokio::time::sleep(CLICK_GAP).await;
    x11::button_up(config, button).await?;
    Ok(OkResponse { ok: true })
}

pub async fn press(config: &CuaConfig, req: &ClickRequest) -> Result<OkResponse, CuaError> {
    ensure_ready(config)?;
    validate_point(config, req.x, req.y)?;
    let button = validate_button(req.button)?;
    let _g = lock_actuator(config).await?;
    x11::pointer_press(config, req.x, req.y, button).await?;
    Ok(OkResponse { ok: true })
}

pub async fn release(config: &CuaConfig, req: &ReleaseRequest) -> Result<OkResponse, CuaError> {
    release_step(config, req.x, req.y, req.button, req.path.as_deref()).await
}

pub(crate) async fn release_step(
    config: &CuaConfig,
    x: Option<i32>,
    y: Option<i32>,
    button: Option<u8>,
    path: Option<&[CuaPoint]>,
) -> Result<OkResponse, CuaError> {
    ensure_ready(config)?;
    let button = validate_button(button)?;
    match (x, y) {
        (Some(x), Some(y)) => validate_point(config, x, y)?,
        (None, None) => {}
        _ => {
            return Err(CuaError::Invalid(
                "x and y must both be set or both omitted".into(),
            ));
        }
    }
    if let Some(path) = path {
        if path.len() > MAX_MOTION_PATH {
            return Err(CuaError::Invalid(format!(
                "release path has {} points; max is {MAX_MOTION_PATH}",
                path.len()
            )));
        }
        for point in path {
            validate_point(config, point.x, point.y)?;
        }
    }
    let extra = match (x, y) {
        (Some(px), Some(py)) => Some((px, py)),
        _ => None,
    };
    let _g = lock_actuator(config).await?;
    release_inner(config, extra, path, button).await
}

pub(crate) async fn release_inner(
    config: &CuaConfig,
    extra: Option<(i32, i32)>,
    path: Option<&[CuaPoint]>,
    button: u8,
) -> Result<OkResponse, CuaError> {
    match x11::motion_path_and_release(config, &[], extra, path, button).await {
        Ok(()) => {}
        // xdotool drives the same X server, so a wedge is not retryable.
        Err(err @ CuaError::DisplayDown(_)) => return Err(err),
        Err(_) => {
            xdotool::xdotool_owned(config, &pointer_release_args(path, extra, button)).await?;
        }
    }
    Ok(OkResponse { ok: true })
}

pub async fn type_text(config: &CuaConfig, req: &TypeRequest) -> Result<OkResponse, CuaError> {
    type_text_inner(config, &req.text).await
}

pub(crate) async fn type_text_inner(
    config: &CuaConfig,
    text: &str,
) -> Result<OkResponse, CuaError> {
    ensure_ready(config)?;
    if text.is_empty() {
        return Err(CuaError::Invalid("text must not be empty".into()));
    }
    if text.len() > 16 * 1024 {
        return Err(CuaError::Invalid("text is too long".into()));
    }
    let _g = lock_actuator(config).await?;
    x11::type_text(config, text).await?;
    Ok(OkResponse { ok: true })
}

pub async fn key(config: &CuaConfig, req: &KeyRequest) -> Result<OkResponse, CuaError> {
    key_inner(config, &req.key, req.action.unwrap_or(KeyAction::Tap)).await
}

pub(crate) async fn key_inner(
    config: &CuaConfig,
    key: &str,
    action: KeyAction,
) -> Result<OkResponse, CuaError> {
    ensure_ready(config)?;
    validate_key(key)?;
    let _g = lock_actuator(config).await?;
    x11::key(config, key, action).await?;
    Ok(OkResponse { ok: true })
}

pub async fn scroll(config: &CuaConfig, req: &ScrollRequest) -> Result<OkResponse, CuaError> {
    ensure_ready(config)?;
    validate_point(config, req.x, req.y)?;
    if req.dx == 0 && req.dy == 0 {
        return Err(CuaError::Invalid("dx and dy must not both be 0".into()));
    }
    let _g = lock_actuator(config).await?;
    x11::scroll(config, req.x, req.y, req.dx, req.dy).await?;
    Ok(OkResponse { ok: true })
}

pub async fn double_click(config: &CuaConfig, req: &ClickRequest) -> Result<OkResponse, CuaError> {
    ensure_ready(config)?;
    validate_point(config, req.x, req.y)?;
    let button = validate_button(req.button)?;
    let _g = lock_actuator(config).await?;
    x11::pointer_press(config, req.x, req.y, button).await?;
    tokio::time::sleep(CLICK_GAP).await;
    x11::button_up(config, button).await?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    x11::button_click(config, button).await?;
    Ok(OkResponse { ok: true })
}

pub async fn move_pointer(config: &CuaConfig, req: &MoveRequest) -> Result<OkResponse, CuaError> {
    ensure_ready(config)?;
    validate_point(config, req.x, req.y)?;
    let _g = lock_actuator(config).await?;
    x11::move_pointer(config, req.x, req.y).await?;
    Ok(OkResponse { ok: true })
}

pub async fn drag(config: &CuaConfig, req: &DragRequest) -> Result<OkResponse, CuaError> {
    ensure_ready(config)?;
    validate_point(config, req.x1, req.y1)?;
    validate_point(config, req.x2, req.y2)?;
    let button = validate_button(req.button)?;
    let points = drag_waypoints(req.x1, req.y1, req.x2, req.y2);
    let _g = lock_actuator(config).await?;
    // SAFETY: `drag_waypoints` always emits at least the start point.
    let (x1, y1) = unsafe { *points.get_unchecked(0) };
    x11::pointer_press(config, x1, y1, button).await?;
    tokio::time::sleep(DRAG_GRAB).await;
    match x11::motion_path_and_release(config, &points[1..], None, None, button).await {
        Ok(()) => {}
        // xdotool drives the same X server, so a wedge is not retryable.
        Err(err @ CuaError::DisplayDown(_)) => return Err(err),
        Err(_) => {
            let mut args = SmallVec::<[String; 16]>::new();
            for &(x, y) in points.iter().skip(1) {
                args.push("mousemove".into());
                args.push(x.to_string());
                args.push(y.to_string());
            }
            args.push("mouseup".into());
            args.push(button.to_string());
            xdotool::xdotool_owned(config, &args).await?;
        }
    }
    Ok(OkResponse { ok: true })
}

pub(crate) fn ensure_ready(config: &CuaConfig) -> Result<(), CuaError> {
    if !config.enabled {
        return Err(CuaError::Disabled);
    }
    if !display_up(&config.display) {
        return Err(CuaError::DisplayDown(config.display.clone()));
    }
    Ok(())
}

/// In the box. Outside it there's no one else's name to take.
pub(crate) fn verify_servers() -> bool {
    env::var("BOX_RUN_AS").as_deref() == Ok("agents")
}

fn display_up(display: &str) -> bool {
    let num = display
        .trim()
        .trim_start_matches(':')
        .split('.')
        .next()
        .unwrap_or("1");
    std::path::Path::new("/tmp/.X11-unix")
        .join(format!("X{num}"))
        .exists()
}

pub(crate) fn pointer_press_args(x: i32, y: i32, button: u8) -> SmallVec<[String; 16]> {
    smallvec::smallvec![
        "mousemove".into(),
        x.to_string(),
        y.to_string(),
        "mousedown".into(),
        button.to_string(),
    ]
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn pointer_click_args(x: i32, y: i32, button: u8) -> SmallVec<[String; 16]> {
    let mut args = pointer_press_args(x, y, button);
    args.push("mouseup".into());
    args.push(button.to_string());
    args
}

pub(crate) fn pointer_release_args(
    path: Option<&[CuaPoint]>,
    extra: Option<(i32, i32)>,
    button: u8,
) -> SmallVec<[String; 16]> {
    let mut args = SmallVec::new();
    if let Some(path) = path {
        for point in path {
            args.push("mousemove".into());
            args.push(point.x.to_string());
            args.push(point.y.to_string());
        }
    }
    if let Some((x, y)) = extra {
        args.push("mousemove".into());
        args.push(x.to_string());
        args.push(y.to_string());
    }
    args.push("mouseup".into());
    args.push(button.to_string());
    args
}

/// X11 wheel: 120 units = one notch. Smaller deltas still emit one notch.
pub fn wheel_ticks(delta: i32) -> u32 {
    let abs = delta.unsigned_abs();
    if abs == 0 {
        return 0;
    }
    ((abs + 119) / 120).clamp(1, 12)
}

pub(crate) fn scroll_args(x: i32, y: i32, dx: i32, dy: i32) -> SmallVec<[String; 16]> {
    let mut args = smallvec::smallvec!["mousemove".into(), x.to_string(), y.to_string()];
    fn append_ticks(args: &mut SmallVec<[String; 16]>, button: u8, ticks: u32) {
        for _ in 0..ticks {
            args.push("mousedown".into());
            args.push(button.to_string());
            args.push("mouseup".into());
            args.push(button.to_string());
        }
    }
    // X buttons: 4=up, 5=down, 6=left, 7=right
    if dy != 0 {
        let button = if dy > 0 { 5_u8 } else { 4_u8 };
        append_ticks(&mut args, button, wheel_ticks(dy));
    }
    if dx != 0 {
        let button = if dx > 0 { 7_u8 } else { 6_u8 };
        append_ticks(&mut args, button, wheel_ticks(dx));
    }
    args
}

pub fn drag_waypoints(
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32,
) -> ArrayVec<(i32, i32), MAX_DRAG_WAYPOINTS> {
    let mut out = ArrayVec::new();
    if x1 == x2 && y1 == y2 {
        // SAFETY: capacity is 25; this is the first element.
        unsafe { out.push_unchecked((x1, y1)) };
        return out;
    }
    let dx = f64::from(x2 - x1);
    let dy = f64::from(y2 - y1);
    let dist = (dx * dx + dy * dy).sqrt();
    // Openbox ignores a single warp while Button1 is held. Always emit
    // several MotionNotify events, even for a short title-bar drag.
    let steps = ((dist / 40.0).ceil() as i32).clamp(8, 24);
    for i in 0..=steps {
        let t = f64::from(i) / f64::from(steps);
        let pt = (
            (f64::from(x1) + dx * t).round() as i32,
            (f64::from(y1) + dy * t).round() as i32,
        );
        // SAFETY: `steps` is 8..=24 so this loop writes at most 25 points.
        unsafe { out.push_unchecked(pt) };
    }
    if let Some(last) = out.last_mut() {
        *last = (x2, y2);
    }
    out
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_request_may_name_another_screen_of_the_box() {
        let shared = CuaConfig {
            display: ":1".into(),
            width: 1280,
            height: 800,
            enabled: true,
        };
        assert_eq!(shared.on(None).unwrap().display, ":1");
        assert_eq!(shared.on(Some("")).unwrap().display, ":1");
        let own = shared.on(Some(":3")).unwrap();
        assert_eq!(
            (own.display.as_str(), own.width, own.height),
            (":3", 1280, 800)
        );
        for bad in ["3", ":", ":1.0", ":abc", ":1234", "; rm -rf /"] {
            assert!(shared.on(Some(bad)).is_err(), "{bad:?}");
        }
    }

    use super::*;

    #[test]
    fn rejects_out_of_range() {
        let cfg = CuaConfig::disabled();
        let mut cfg = cfg;
        cfg.enabled = true;
        assert!(validate_point(&cfg, 0, 0).is_ok());
        assert!(validate_point(&cfg, 1279, 799).is_ok());
        assert!(validate_point(&cfg, 1280, 0).is_err());
        assert!(validate_point(&cfg, -1, 10).is_err());
        assert!(validate_point(&cfg, 10, 800).is_err());
    }

    #[test]
    fn display_geom_drives_coordinate_space() {
        let mut cfg = CuaConfig::disabled();
        cfg.enabled = true;
        cfg.width = 1920;
        cfg.height = 1080;
        assert!(validate_point(&cfg, 1919, 1079).is_ok());
        assert!(validate_point(&cfg, 1920, 0).is_err());
        assert!(validate_point(&cfg, 0, 1080).is_err());
    }

    #[test]
    fn click_args_are_press_release_without_sync() {
        let args = pointer_click_args(10, 20, 1);
        assert_eq!(
            args.as_slice(),
            ["mousemove", "10", "20", "mousedown", "1", "mouseup", "1"]
        );
        assert!(!args.iter().any(|a| a == "--sync"));
        assert!(!args.iter().any(|a| a == "click"));
        assert_eq!(
            pointer_press_args(10, 20, 3).as_slice(),
            ["mousemove", "10", "20", "mousedown", "3"]
        );
    }

    #[test]
    fn scroll_uses_wheel_press_release_not_xdotool_click() {
        let args = scroll_args(640, 400, 0, 120);
        assert_eq!(
            args.as_slice(),
            ["mousemove", "640", "400", "mousedown", "5", "mouseup", "5"]
        );
        assert!(!args.iter().any(|a| a == "click"));
        assert!(!args.iter().any(|a| a == "--sync"));
        assert_eq!(wheel_ticks(120), 1);
        assert_eq!(wheel_ticks(1), 1);
        assert_eq!(wheel_ticks(240), 2);
        assert_eq!(wheel_ticks(-120), 1);
        let left = scroll_args(10, 10, -120, 0);
        assert!(left.contains(&"6".into()));
        assert!(!left.iter().any(|a| a == "click"));
    }

    #[test]
    fn drag_waypoints_include_intermediates_and_exact_end() {
        let points = drag_waypoints(0, 0, 80, 0);
        assert_eq!(points.first().copied(), Some((0, 0)));
        assert_eq!(points.last().copied(), Some((80, 0)));
        assert!(points.len() >= 3);
        let same = drag_waypoints(10, 10, 10, 10);
        assert_eq!(same.as_slice(), [(10, 10)]);
        let short = drag_waypoints(0, 0, 20, 0);
        assert!(
            short.len() >= 9,
            "short drags still need several held moves, got {short:?}"
        );
        assert_eq!(short.last().copied(), Some((20, 0)));
        let release = pointer_release_args(
            Some(&[CuaPoint { x: 20, y: 20 }, CuaPoint { x: 30, y: 30 }]),
            Some((40, 50)),
            1,
        );
        assert_eq!(
            release.as_slice(),
            [
                "mousemove",
                "20",
                "20",
                "mousemove",
                "30",
                "30",
                "mousemove",
                "40",
                "50",
                "mouseup",
                "1"
            ]
        );
        assert!(!release.iter().any(|a| a == "click" || a == "--sync"));
        let click_up = pointer_release_args(None, None, 1);
        assert_eq!(click_up.as_slice(), ["mouseup", "1"]);
    }

    #[test]
    fn disabled_screenshot_errors() {
        let cfg = CuaConfig::disabled();
        let err = futures_error(&cfg);
        assert!(matches!(err, CuaError::Disabled));
        assert_eq!(std::mem::size_of::<CuaConfig>(), 40);
        assert!(!cfg.capability_ready());
    }

    fn futures_error(cfg: &CuaConfig) -> CuaError {
        ensure_ready(cfg).unwrap_err()
    }

    /// Time is paused, so this asserts the shape of the wait, not its length.
    #[tokio::test(start_paused = true)]
    async fn actuator_refuses_to_queue_forever() {
        let mut cfg = CuaConfig::disabled();
        cfg.enabled = true;
        let held = ACTUATOR.lock().await;
        let err = lock_actuator(&cfg)
            .await
            .expect_err("a held actuator must expire, not queue");
        // 503, not 502: the desk is stuck, the request was not malformed.
        assert!(matches!(err, CuaError::DisplayDown(_)), "got {err:?}");
        drop(held);
        assert!(
            lock_actuator(&cfg).await.is_ok(),
            "released lock is takeable"
        );
    }
}
