//! One-call CUA recipes.
//!
//! reverse-web-mcp compiles an app's wants into a DAG and runs it without
//! another model turn. buwiz-computer is the *screen*, not that compiler: the caller
//! already planned the clicks. This module runs that plan in **one** HTTP
//! request so the agent does not pay a round trip per `click` / `scroll`.
//!
//! The pointer is shared, so steps are sequential. There is no parallel CUA.

use std::env;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use box_desktop::{screen_workspace, slots_dir};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::cook_record::{recording_mime, start_cook_recorder, stop_cook_recorder, CookRecorder};
use crate::observe::{self, RecipeObserve, StepObservation};
use crate::settle::{dock_click, page_snap, wait_chromium_usable, wait_page_change};
use crate::{
    click, double_click, drag, key_inner, move_pointer, press, release_step, screenshot_from_png,
    screenshot_png, scroll, type_text_inner, ClickRequest, CuaConfig, CuaError, CuaPoint,
    DragRequest, KeyAction, MoveRequest, ScreenshotResponse, ScrollRequest,
};

pub const MAX_RECIPE_STEPS: usize = 256;
pub const MAX_WAIT_MS: u64 = 10_000;
const RESET_DESKTOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(8);
const TYPE_CHAR_MS: u64 = 30;
const KEY_PACE_MS: u64 = 25;
const OMNIBOX_SETTLE_MS: u64 = 220;
const RAW_APP_WAIT_MS: u64 = 6_000;
const RAW_PAGE_WAIT_MS: u64 = 5_000;
const COMPRESSED_APP_WAIT_MS: u64 = 4_000;
const COMPRESSED_PAGE_WAIT_MS: u64 = 3_000;
/// Let tint2 finish a dock toggle (hide) before we map Chromium again.
const DOCK_TOGGLE_MS: u64 = 180;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecipeScreenshot {
    None,
    #[default]
    End,
    Each,
}

/// How long cook should wait for Chromium / page paint. Default is **off**
/// (no extra Chromium/page waits, no launch/close side effects).
/// `compressed` / `raw` are opt-in smart waits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecipeSettle {
    #[default]
    Off,
    Compressed,
    Raw,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum RecipeStep {
    Click {
        x: i32,
        y: i32,
        #[serde(default)]
        button: Option<u8>,
    },
    #[serde(alias = "double-click")]
    DoubleClick {
        x: i32,
        y: i32,
        #[serde(default)]
        button: Option<u8>,
    },
    Move {
        x: i32,
        y: i32,
    },
    #[serde(alias = "mousedown")]
    Press {
        x: i32,
        y: i32,
        #[serde(default)]
        button: Option<u8>,
    },
    #[serde(alias = "mouseup")]
    Release {
        #[serde(default)]
        x: Option<i32>,
        #[serde(default)]
        y: Option<i32>,
        #[serde(default)]
        button: Option<u8>,
        #[serde(default)]
        path: Option<Vec<CuaPoint>>,
    },
    Drag {
        x1: i32,
        y1: i32,
        x2: i32,
        y2: i32,
        #[serde(default)]
        button: Option<u8>,
    },
    Type {
        text: String,
    },
    Key {
        key: String,
        #[serde(default)]
        action: Option<KeyAction>,
    },
    Scroll {
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
    },
    Wait {
        ms: u64,
    },
    Screenshot {},
    /// Close guest windows and leftover jobs. Same X session; not docker restart.
    #[serde(alias = "reset")]
    ResetDesktop {},
}

impl RecipeStep {
    pub fn op_name(&self) -> &'static str {
        match self {
            Self::Click { .. } => "click",
            Self::DoubleClick { .. } => "double_click",
            Self::Move { .. } => "move",
            Self::Press { .. } => "press",
            Self::Release { .. } => "release",
            Self::Drag { .. } => "drag",
            Self::Type { .. } => "type",
            Self::Key { .. } => "key",
            Self::Scroll { .. } => "scroll",
            Self::Wait { .. } => "wait",
            Self::Screenshot {} => "screenshot",
            Self::ResetDesktop {} => "reset_desktop",
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RecipeRequest {
    #[serde(default)]
    pub name: Option<String>,
    /// Default true: stop after the first failing step. Already-run steps stay
    /// in the receipt.
    #[serde(default)]
    pub stop_on_error: Option<bool>,
    #[serde(default)]
    pub screenshot: Option<RecipeScreenshot>,
    /// Capture the framebuffer with ffmpeg/x11grab before the first CUA step
    /// and SIGINT-stop after the last.
    #[serde(default)]
    pub record: Option<bool>,
    /// Workspace-relative or `/workspace/...` dir for PNG/video files. When set,
    /// screenshot JSON omits `png_base64` (path only) so L1 can show artifacts.
    #[serde(default)]
    pub artifact_dir: Option<String>,
    /// Smart waits for a mapped Chromium window and a title/URL change.
    /// Default `off`. Opt in with `compressed` or `raw`. Never launches Chromium.
    #[serde(default)]
    pub settle: Option<RecipeSettle>,
    /// What the receipt reports about the desktop the steps ran against.
    /// Default `off`, which produces the receipt this endpoint produced
    /// before the field existed. `input` adds the window under each pointer
    /// step and the focus before each `type`; `page` adds the URL either
    /// side of the steps that can navigate. See `observe.rs`.
    #[serde(default)]
    pub observe: Option<RecipeObserve>,
    pub steps: Vec<RecipeStep>,
}

impl RecipeRequest {
    pub fn stop_on_error(&self) -> bool {
        self.stop_on_error.unwrap_or(true)
    }

    pub fn screenshot_mode(&self) -> RecipeScreenshot {
        self.screenshot.unwrap_or_default()
    }

    pub fn record(&self) -> bool {
        self.record.unwrap_or(false)
    }

    pub fn settle_mode(&self) -> RecipeSettle {
        self.settle.unwrap_or_default()
    }

    pub fn observe_mode(&self) -> RecipeObserve {
        self.observe.unwrap_or_default()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RecipeStepResult {
    pub index: usize,
    pub op: &'static str,
    /// This step did not return an error. It is not a claim that the step
    /// achieved anything; `observed` is where the box says what it saw.
    pub ok: bool,
    /// Time in the step itself and in any `settle` wait after it. Time spent
    /// observing is not counted here — it is in `observed.observe_ms` — the
    /// same way a per-step screenshot has never been counted here.
    pub ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<ScreenshotResponse>,
    /// What the box saw around this step. Absent means the box did not look:
    /// `observe` was `off`, or the step had nothing to look at. Present with
    /// missing fields means it looked and got no answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed: Option<StepObservation>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecipeArtifact {
    pub kind: &'static str,
    pub label: String,
    pub path: String,
    pub mime: &'static str,
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step_index: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecipeResponse {
    /// No step returned an error. Not a claim that the recipe achieved
    /// anything: see `steps[].observed`.
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Echoed from the request, and absent when it was `off`.
    ///
    /// Without it, a receipt carrying no `observed` blocks is ambiguous: the
    /// box may have looked and seen nothing, or may never have been asked.
    /// A receipt is often read a long way from the request that produced it,
    /// and the whole point here is that a reader should not have to guess.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observe: Option<RecipeObserve>,
    pub ran: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stopped_at: Option<usize>,
    pub duration_ms: u64,
    pub steps: Vec<RecipeStepResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<ScreenshotResponse>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<RecipeArtifact>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recording_error: Option<String>,
}

struct ArtifactSink {
    dir: PathBuf,
    inline_png: bool,
}

impl ArtifactSink {
    async fn save_png(
        &self,
        config: &CuaConfig,
        name: &str,
        png: &[u8],
    ) -> Result<ScreenshotResponse, CuaError> {
        tokio::fs::create_dir_all(&self.dir)
            .await
            .map_err(|err| CuaError::Tool(format!("artifact dir: {err}")))?;
        let dest = self.dir.join(name);
        match tokio::fs::write(&dest, png).await {
            Ok(()) => Ok(screenshot_from_png(
                config,
                png.to_vec(),
                Some(display_workspace_path(&dest)),
                self.inline_png,
            )),
            Err(err) => {
                tracing::warn!(error = %err, path = %dest.display(), "artifact png write failed; inlining");
                Ok(screenshot_from_png(config, png.to_vec(), None, true))
            }
        }
    }
}

fn workspace_root() -> PathBuf {
    env::var("WORKSPACE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            if Path::new("/workspace").is_dir() {
                PathBuf::from("/workspace")
            } else {
                PathBuf::from("./workspace-data")
            }
        })
}

fn display_workspace_path(path: &Path) -> String {
    let root = workspace_root();
    match path.strip_prefix(&root) {
        Ok(rel) => format!("/workspace/{}", rel.display()),
        Err(_) => path.display().to_string(),
    }
}

fn resolve_artifact_dir(raw: &str) -> Result<PathBuf, CuaError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(CuaError::Invalid("artifact_dir must not be empty".into()));
    }
    if trimmed.contains('\0') || trimmed.split('/').any(|part| part == "..") {
        return Err(CuaError::Invalid(
            "artifact_dir must stay inside the workspace".into(),
        ));
    }
    let root = workspace_root();
    let path = Path::new(trimmed);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let root_norm = root.to_string_lossy().trim_end_matches('/').to_string();
    let joined_norm = joined.to_string_lossy().to_string();
    if joined_norm != root_norm && !joined_norm.starts_with(&format!("{root_norm}/")) {
        return Err(CuaError::Invalid(
            "artifact_dir must stay inside the workspace".into(),
        ));
    }
    Ok(joined)
}

/// A cook's video and shots go in the folder of the screen they show.
fn default_artifact_dir(display: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    workspace_root()
        .join(screen_workspace(display, &slots_dir()))
        .join("cooks")
        .join(format!("cook-{stamp}"))
}

fn artifact_from_shot(
    shot: &ScreenshotResponse,
    label: &str,
    step_index: Option<usize>,
) -> Option<RecipeArtifact> {
    let path = shot.path.as_ref()?;
    Some(RecipeArtifact {
        kind: "screenshot",
        label: label.to_string(),
        path: path.clone(),
        mime: "image/png",
        bytes: shot.bytes as u64,
        width: Some(shot.width),
        height: Some(shot.height),
        step_index,
    })
}

async fn capture_shot(
    config: &CuaConfig,
    sink: Option<&ArtifactSink>,
    name: &str,
) -> Result<ScreenshotResponse, CuaError> {
    let png = screenshot_png(config).await?;
    if let Some(sink) = sink {
        sink.save_png(config, name, &png).await
    } else {
        Ok(screenshot_from_png(config, png, None, true))
    }
}

async fn reset_desktop(config: &CuaConfig) -> Result<(), CuaError> {
    let child = Command::new("box-reset-desktop")
        .env("DISPLAY", &config.display)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| {
            CuaError::Tool(format!(
                "reset_desktop: box-reset-desktop is missing ({err}). Rebuild buwiz-computer:local."
            ))
        })?;
    match tokio::time::timeout(RESET_DESKTOP_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(out)) => {
            if out.status.success() {
                Ok(())
            } else {
                let err = String::from_utf8_lossy(&out.stderr);
                Err(CuaError::Tool(format!("reset_desktop failed: {err}")))
            }
        }
        Ok(Err(err)) => Err(CuaError::Tool(format!("reset_desktop: {err}"))),
        Err(_) => Err(CuaError::Tool("reset_desktop timed out after 8s".into())),
    }
}

pub fn validate_recipe(config: &CuaConfig, req: &RecipeRequest) -> Result<(), CuaError> {
    if req.steps.is_empty() {
        return Err(CuaError::Invalid("recipe steps must not be empty".into()));
    }
    if req.steps.len() > MAX_RECIPE_STEPS {
        return Err(CuaError::Invalid(format!(
            "recipe has {} steps; max is {MAX_RECIPE_STEPS}",
            req.steps.len()
        )));
    }
    if let Some(dir) = req.artifact_dir.as_deref() {
        resolve_artifact_dir(dir)?;
    }
    for step in &req.steps {
        match step {
            RecipeStep::Click { x, y, button } | RecipeStep::DoubleClick { x, y, button } => {
                crate::validate_point(config, *x, *y)?;
                if let Some(button) = *button {
                    if !(1..=7).contains(&button) {
                        return Err(CuaError::Invalid("button must be 1-7".into()));
                    }
                }
            }
            RecipeStep::Move { x, y } => crate::validate_point(config, *x, *y)?,
            RecipeStep::Press { x, y, button } => {
                crate::validate_point(config, *x, *y)?;
                if let Some(button) = *button {
                    if !(1..=7).contains(&button) {
                        return Err(CuaError::Invalid("button must be 1-7".into()));
                    }
                }
            }
            RecipeStep::Release { x, y, button, path } => {
                match (x, y) {
                    (Some(x), Some(y)) => crate::validate_point(config, *x, *y)?,
                    (None, None) => {}
                    _ => {
                        return Err(CuaError::Invalid(
                            "x and y must both be set or both omitted".into(),
                        ));
                    }
                }
                if let Some(button) = *button {
                    if !(1..=7).contains(&button) {
                        return Err(CuaError::Invalid("button must be 1-7".into()));
                    }
                }
                if let Some(path) = path {
                    if path.len() > crate::MAX_MOTION_PATH {
                        return Err(CuaError::Invalid(format!(
                            "release path has {} points; max is {}",
                            path.len(),
                            crate::MAX_MOTION_PATH
                        )));
                    }
                    for point in path {
                        crate::validate_point(config, point.x, point.y)?;
                    }
                }
            }
            RecipeStep::Drag {
                x1,
                y1,
                x2,
                y2,
                button,
            } => {
                crate::validate_point(config, *x1, *y1)?;
                crate::validate_point(config, *x2, *y2)?;
                if let Some(button) = *button {
                    if !(1..=7).contains(&button) {
                        return Err(CuaError::Invalid("button must be 1-7".into()));
                    }
                }
            }
            RecipeStep::Type { text } => {
                if text.is_empty() {
                    return Err(CuaError::Invalid("text must not be empty".into()));
                }
                if text.len() > 16 * 1024 {
                    return Err(CuaError::Invalid("text is too long".into()));
                }
            }
            RecipeStep::Key { key, action: _ } => {
                if key.is_empty() {
                    return Err(CuaError::Invalid("key must not be empty".into()));
                }
                if key.chars().any(|c| c.is_whitespace() || c == ';') {
                    return Err(CuaError::Invalid("key contains invalid characters".into()));
                }
            }
            RecipeStep::Scroll { x, y, dx, dy } => {
                crate::validate_point(config, *x, *y)?;
                if *dx == 0 && *dy == 0 {
                    return Err(CuaError::Invalid("dx and dy must not both be 0".into()));
                }
            }
            RecipeStep::Wait { ms } => {
                if *ms > MAX_WAIT_MS {
                    return Err(CuaError::Invalid(format!(
                        "wait {ms}ms exceeds max {MAX_WAIT_MS}ms"
                    )));
                }
            }
            RecipeStep::Screenshot {} | RecipeStep::ResetDesktop {} => {}
        }
    }
    Ok(())
}

pub async fn run_recipe(
    config: &CuaConfig,
    req: &RecipeRequest,
) -> Result<RecipeResponse, CuaError> {
    validate_recipe(config, req)?;
    crate::ensure_ready(config)?;

    let sink = if req.record() || req.artifact_dir.is_some() {
        let dir = if let Some(raw) = req.artifact_dir.as_deref() {
            resolve_artifact_dir(raw)?
        } else {
            default_artifact_dir(&config.display)
        };
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|err| CuaError::Tool(format!("artifact dir: {err}")))?;
        Some(ArtifactSink {
            dir,
            inline_png: false,
        })
    } else {
        None
    };

    // ffmpeg x11grab must be grabbing before the first CUA step (including
    // warmup_pointer). Stop is SIGINT after the last step / end screenshot.
    let mut recorder: Option<CookRecorder> = None;
    let mut recording_error = None;
    if req.record() {
        if let Some(sink) = sink.as_ref() {
            match start_cook_recorder(
                &config.display,
                config.width,
                config.height,
                sink.dir.join("cook.mp4"),
            )
            .await
            {
                Ok(rec) => recorder = Some(rec),
                Err(err) => {
                    tracing::warn!(error = %err, "cook recording did not start");
                    recording_error = Some(err.to_string());
                }
            }
        }
    }

    // Non-sync warp so step 0 is not the first XTEST motion on this display.
    crate::warmup_pointer(config).await;

    let started = Instant::now();
    let stop_on_error = req.stop_on_error();
    let shot_mode = req.screenshot_mode();
    let settle = req.settle_mode();
    let observe = req.observe_mode();
    let mut steps = Vec::with_capacity(req.steps.len());
    let mut artifacts = Vec::new();
    let mut ok = true;
    let mut stopped_at = None;
    let mut ctrl_held = false;
    let mut last_key: Option<String> = None;

    for (index, step) in req.steps.iter().enumerate() {
        // Read what the step is about to act on before it acts: a click that
        // raises a window has already changed what is under the pointer by
        // the time it returns, and the question the caller has is what the
        // click landed on.
        let watch = observe::watch(observe, step);
        let mut observed = match watch.as_ref() {
            Some(watch) => Some(watch.before(config).await),
            None => None,
        };
        let before_page = match (settle, step) {
            (RecipeSettle::Off, _) => None,
            (_, RecipeStep::Key { key, .. }) if is_return_key(key) => Some(page_snap(config).await),
            _ => None,
        };
        let step_started = Instant::now();
        let result = run_step(config, step).await;
        if result.is_ok() && settle != RecipeSettle::Off {
            settle_after(
                config,
                step,
                settle,
                &mut ctrl_held,
                &mut last_key,
                before_page.as_ref(),
            )
            .await;
        } else {
            track_key_state(step, &mut ctrl_held, &mut last_key);
        }
        let ms = step_started.elapsed().as_millis() as u64;
        // After `settle`, so a recipe that already waited for a navigation
        // gets the page it waited for rather than the one it left.
        if let (Some(watch), Some(obs)) = (watch.as_ref(), observed.as_mut()) {
            watch.after(obs).await;
        }
        match result {
            Ok(()) => {
                tracing::info!(index, op = step.op_name(), ms, ok = true, "recipe step");
                let want_shot = matches!(step, RecipeStep::Screenshot {})
                    || (shot_mode == RecipeScreenshot::Each
                        && !matches!(step, RecipeStep::Wait { .. }));
                let screenshot = if want_shot {
                    match capture_shot(config, sink.as_ref(), &format!("step-{index:02}.png")).await
                    {
                        Ok(shot) => Some(shot),
                        Err(err) if matches!(step, RecipeStep::Screenshot {}) => {
                            ok = false;
                            steps.push(RecipeStepResult {
                                index,
                                op: step.op_name(),
                                ok: false,
                                ms,
                                error: Some(err.to_string()),
                                screenshot: None,
                                observed: observed.take(),
                            });
                            if stop_on_error {
                                stopped_at = Some(index);
                                break;
                            }
                            continue;
                        }
                        Err(_) => None,
                    }
                } else {
                    None
                };
                if let Some(shot) = screenshot.as_ref() {
                    if let Some(art) =
                        artifact_from_shot(shot, &format!("step {index}"), Some(index))
                    {
                        artifacts.push(art);
                    }
                }
                steps.push(RecipeStepResult {
                    index,
                    op: step.op_name(),
                    ok: true,
                    ms,
                    error: None,
                    screenshot,
                    observed,
                });
            }
            Err(err) => {
                tracing::info!(
                    index,
                    op = step.op_name(),
                    ms,
                    ok = false,
                    error = %err,
                    "recipe step"
                );
                ok = false;
                steps.push(RecipeStepResult {
                    index,
                    op: step.op_name(),
                    ok: false,
                    ms,
                    error: Some(err.to_string()),
                    screenshot: None,
                    observed,
                });
                if stop_on_error {
                    stopped_at = Some(index);
                    break;
                }
            }
        }
    }

    let mut final_shot = None;
    if ok && shot_mode == RecipeScreenshot::End {
        match capture_shot(config, sink.as_ref(), "end.png").await {
            Ok(shot) => {
                if let Some(art) = artifact_from_shot(&shot, "end", None) {
                    artifacts.push(art);
                }
                final_shot = Some(shot);
            }
            Err(err) => {
                ok = false;
                steps.push(RecipeStepResult {
                    index: steps.len(),
                    op: "screenshot",
                    ok: false,
                    ms: 0,
                    error: Some(err.to_string()),
                    screenshot: None,
                    // The end screenshot is not one of the recipe's steps, so
                    // there was never anything about it to observe.
                    observed: None,
                });
                stopped_at = Some(steps.len().saturating_sub(1));
            }
        }
    }

    if let Some(rec) = recorder.take() {
        let rec_path = rec.path.clone();
        match stop_cook_recorder(rec).await {
            Ok(path) => {
                let bytes = tokio::fs::metadata(&path)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0);
                artifacts.push(RecipeArtifact {
                    kind: "recording",
                    label: "cook".into(),
                    path: display_workspace_path(&path),
                    mime: recording_mime(&path),
                    bytes,
                    width: Some(config.width),
                    height: Some(config.height),
                    step_index: None,
                });
            }
            Err(err) => {
                tracing::warn!(error = %err, path = %rec_path.display(), "cook recording stop failed");
                recording_error = Some(err.to_string());
            }
        }
    }

    let duration_ms = started.elapsed().as_millis() as u64;
    tracing::info!(
        ok,
        duration_ms,
        ran = steps.iter().filter(|s| s.ok).count(),
        "recipe done"
    );

    Ok(RecipeResponse {
        ok,
        name: req.name.clone(),
        observe: (observe != RecipeObserve::Off).then_some(observe),
        ran: steps.iter().filter(|s| s.ok).count(),
        stopped_at,
        duration_ms,
        steps,
        screenshot: final_shot,
        artifacts,
        recording_error,
    })
}

async fn run_step(config: &CuaConfig, step: &RecipeStep) -> Result<(), CuaError> {
    match step {
        RecipeStep::Click { x, y, button } => {
            click(
                config,
                &ClickRequest {
                    x: *x,
                    y: *y,
                    button: *button,
                },
            )
            .await?;
        }
        RecipeStep::DoubleClick { x, y, button } => {
            double_click(
                config,
                &ClickRequest {
                    x: *x,
                    y: *y,
                    button: *button,
                },
            )
            .await?;
        }
        RecipeStep::Move { x, y } => {
            move_pointer(config, &MoveRequest { x: *x, y: *y }).await?;
        }
        RecipeStep::Press { x, y, button } => {
            press(
                config,
                &ClickRequest {
                    x: *x,
                    y: *y,
                    button: *button,
                },
            )
            .await?;
        }
        RecipeStep::Release { x, y, button, path } => {
            release_step(config, *x, *y, *button, path.as_deref()).await?;
        }
        RecipeStep::Drag {
            x1,
            y1,
            x2,
            y2,
            button,
        } => {
            drag(
                config,
                &DragRequest {
                    x1: *x1,
                    y1: *y1,
                    x2: *x2,
                    y2: *y2,
                    button: *button,
                },
            )
            .await?;
        }
        RecipeStep::Type { text } => {
            let total = text.chars().count();
            for (typed, ch) in text.chars().enumerate() {
                let mut buf = [0u8; 4];
                let piece = ch.encode_utf8(&mut buf);
                if let Err(err) = type_text_inner(config, piece).await {
                    // `mundo` arriving as `mu` is the failure this reports.
                    // The step result is all the caller gets, so it has to
                    // say how much of the string reached the X server.
                    return Err(CuaError::Tool(format!(
                        "type stopped after {typed} of {total} characters: {err}"
                    )));
                }
                tokio::time::sleep(std::time::Duration::from_millis(TYPE_CHAR_MS)).await;
            }
        }
        RecipeStep::Key {
            key: keysym,
            action,
        } => {
            key_inner(config, keysym, action.unwrap_or_default()).await?;
            tokio::time::sleep(std::time::Duration::from_millis(KEY_PACE_MS)).await;
        }
        RecipeStep::Scroll { x, y, dx, dy } => {
            scroll(
                config,
                &ScrollRequest {
                    x: *x,
                    y: *y,
                    dx: *dx,
                    dy: *dy,
                },
            )
            .await?;
        }
        RecipeStep::Wait { ms } => {
            if *ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(*ms)).await;
            }
        }
        RecipeStep::Screenshot {} => {}
        RecipeStep::ResetDesktop {} => {
            reset_desktop(config).await?;
        }
    }
    Ok(())
}

fn is_return_key(key: &str) -> bool {
    key.eq_ignore_ascii_case("Return") || key.eq_ignore_ascii_case("Enter")
}

fn is_ctrl_key(key: &str) -> bool {
    key.eq_ignore_ascii_case("ctrl") || key.eq_ignore_ascii_case("control")
}

fn track_key_state(step: &RecipeStep, ctrl_held: &mut bool, last_key: &mut Option<String>) {
    if let RecipeStep::Key { key, action } = step {
        let act = action.unwrap_or_default();
        if is_ctrl_key(key) {
            match act {
                KeyAction::Down => *ctrl_held = true,
                KeyAction::Up => *ctrl_held = false,
                KeyAction::Tap => {}
            }
        }
        *last_key = Some(key.clone());
    } else if !matches!(step, RecipeStep::Wait { .. }) {
        *last_key = None;
        if matches!(
            step,
            RecipeStep::Type { .. } | RecipeStep::Click { .. } | RecipeStep::ResetDesktop {}
        ) {
            *ctrl_held = false;
        }
    }
}

fn app_wait_ms(settle: RecipeSettle) -> u64 {
    match settle {
        RecipeSettle::Raw => RAW_APP_WAIT_MS,
        RecipeSettle::Compressed => COMPRESSED_APP_WAIT_MS,
        RecipeSettle::Off => 0,
    }
}

fn page_wait_ms(settle: RecipeSettle) -> u64 {
    match settle {
        RecipeSettle::Raw => RAW_PAGE_WAIT_MS,
        RecipeSettle::Compressed => COMPRESSED_PAGE_WAIT_MS,
        RecipeSettle::Off => 0,
    }
}

async fn settle_after(
    config: &CuaConfig,
    step: &RecipeStep,
    settle: RecipeSettle,
    ctrl_held: &mut bool,
    last_key: &mut Option<String>,
    before_page: Option<&crate::settle::PageSnap>,
) {
    match step {
        RecipeStep::Click { y, .. } | RecipeStep::DoubleClick { y, .. } => {
            if dock_click(*y, config.height) {
                tokio::time::sleep(std::time::Duration::from_millis(DOCK_TOGGLE_MS)).await;
                let _ = wait_chromium_usable(config, app_wait_ms(settle)).await;
            }
        }
        RecipeStep::Key { key, action } => {
            let act = action.unwrap_or_default();
            if key.eq_ignore_ascii_case("ctrl+l")
                || (key.eq_ignore_ascii_case("l") && *ctrl_held)
                || (is_ctrl_key(key)
                    && matches!(act, KeyAction::Up)
                    && last_key
                        .as_deref()
                        .is_some_and(|k| k.eq_ignore_ascii_case("l")))
            {
                tokio::time::sleep(std::time::Duration::from_millis(OMNIBOX_SETTLE_MS)).await;
            }
            if is_return_key(key) {
                if let Some(before) = before_page {
                    let _ = wait_page_change(config, before, page_wait_ms(settle)).await;
                }
            }
        }
        _ => {}
    }
    track_key_state(step, ctrl_held, last_key);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> CuaConfig {
        let mut cfg = CuaConfig::disabled();
        cfg.enabled = true;
        cfg
    }

    #[test]
    fn rejects_empty() {
        let req = RecipeRequest {
            steps: vec![],
            ..RecipeRequest::default()
        };
        assert!(matches!(
            validate_recipe(&cfg(), &req),
            Err(CuaError::Invalid(_))
        ));
    }

    #[test]
    fn rejects_too_many_steps() {
        let step = RecipeStep::Wait { ms: 1 };
        let req = RecipeRequest {
            screenshot: Some(RecipeScreenshot::None),
            steps: vec![step; MAX_RECIPE_STEPS + 1],
            ..RecipeRequest::default()
        };
        assert!(validate_recipe(&cfg(), &req).is_err());
    }

    #[test]
    fn rejects_out_of_range_before_run() {
        let req = RecipeRequest {
            screenshot: Some(RecipeScreenshot::None),
            steps: vec![
                RecipeStep::Move { x: 0, y: 0 },
                RecipeStep::Click {
                    x: 1280,
                    y: 0,
                    button: None,
                },
            ],
            ..RecipeRequest::default()
        };
        assert!(matches!(
            validate_recipe(&cfg(), &req),
            Err(CuaError::OutOfRange(1280, 0, 1280, 800))
        ));
    }

    #[test]
    fn rejects_long_wait() {
        let req = RecipeRequest {
            screenshot: Some(RecipeScreenshot::None),
            steps: vec![RecipeStep::Wait {
                ms: MAX_WAIT_MS + 1,
            }],
            ..RecipeRequest::default()
        };
        assert!(validate_recipe(&cfg(), &req).is_err());
    }

    #[test]
    fn parses_double_click_alias() {
        let step: RecipeStep =
            serde_json::from_str(r#"{"op":"double-click","x":1,"y":2}"#).unwrap();
        assert!(matches!(step, RecipeStep::DoubleClick { x: 1, y: 2, .. }));
        let step: RecipeStep =
            serde_json::from_str(r#"{"op":"double_click","x":1,"y":2}"#).unwrap();
        assert!(matches!(step, RecipeStep::DoubleClick { .. }));
    }

    #[test]
    fn parses_press_release_and_key_action() {
        let press: RecipeStep =
            serde_json::from_str(r#"{"op":"press","x":10,"y":20,"button":1}"#).unwrap();
        assert!(matches!(press, RecipeStep::Press { x: 10, y: 20, .. }));
        let release: RecipeStep =
            serde_json::from_str(r#"{"op":"release","x":40,"y":50}"#).unwrap();
        assert!(matches!(
            release,
            RecipeStep::Release {
                x: Some(40),
                y: Some(50),
                ..
            }
        ));
        let key_down: RecipeStep =
            serde_json::from_str(r#"{"op":"key","key":"shift","action":"down"}"#).unwrap();
        assert!(matches!(
            key_down,
            RecipeStep::Key {
                action: Some(KeyAction::Down),
                ..
            }
        ));
    }

    #[test]
    fn parses_reset_desktop_aliases() {
        let step: RecipeStep = serde_json::from_str(r#"{"op":"reset_desktop"}"#).unwrap();
        assert!(matches!(step, RecipeStep::ResetDesktop {}));
        let step: RecipeStep = serde_json::from_str(r#"{"op":"reset"}"#).unwrap();
        assert!(matches!(step, RecipeStep::ResetDesktop {}));
        assert_eq!(step.op_name(), "reset_desktop");
    }

    #[test]
    fn rejects_artifact_dir_escape() {
        let req = RecipeRequest {
            artifact_dir: Some("../etc".into()),
            steps: vec![RecipeStep::Wait { ms: 1 }],
            ..RecipeRequest::default()
        };
        assert!(validate_recipe(&cfg(), &req).is_err());
    }

    #[test]
    fn accepts_workspace_artifact_dir() {
        let req = RecipeRequest {
            artifact_dir: Some(".l1/cooks/demo".into()),
            record: Some(true),
            steps: vec![RecipeStep::ResetDesktop {}, RecipeStep::Wait { ms: 1 }],
            ..RecipeRequest::default()
        };
        assert!(validate_recipe(&cfg(), &req).is_ok());
    }

    #[test]
    fn parses_settle_raw() {
        let req: RecipeRequest =
            serde_json::from_str(r#"{"settle":"raw","steps":[{"op":"wait","ms":1}]}"#).unwrap();
        assert_eq!(req.settle_mode(), RecipeSettle::Raw);
        let req: RecipeRequest =
            serde_json::from_str(r#"{"steps":[{"op":"wait","ms":1}]}"#).unwrap();
        assert_eq!(req.settle_mode(), RecipeSettle::Off);
    }

    /// Recipes are stored as immutable JSON and replayed long after they were
    /// taped. One taped before `observe` existed has to keep meaning what it
    /// meant, which is: do not observe.
    #[test]
    fn a_recipe_taped_before_observe_existed_still_parses_and_observes_nothing() {
        let stored = r#"{
            "name": "kabisado",
            "steps": [
                {"op": "click", "x": 575, "y": 751},
                {"op": "wait", "ms": 4158},
                {"op": "type", "text": "youtube.com"},
                {"op": "key", "key": "Return"}
            ]
        }"#;
        let req: RecipeRequest = serde_json::from_str(stored).unwrap();
        assert_eq!(req.observe_mode(), RecipeObserve::Off);
        assert_eq!(req.steps.len(), 4);
    }

    #[test]
    fn parses_observe_modes() {
        let req: RecipeRequest =
            serde_json::from_str(r#"{"observe":"page","steps":[{"op":"wait","ms":1}]}"#).unwrap();
        assert_eq!(req.observe_mode(), RecipeObserve::Page);
        let req: RecipeRequest =
            serde_json::from_str(r#"{"observe":"input","steps":[{"op":"wait","ms":1}]}"#).unwrap();
        assert_eq!(req.observe_mode(), RecipeObserve::Input);
    }

    /// Existing clients read these receipts. With `observe` off the box must
    /// not put a single new byte on the wire.
    #[test]
    fn a_receipt_without_observation_is_the_receipt_clients_already_parse() {
        let step = RecipeStepResult {
            index: 0,
            op: "click",
            ok: true,
            ms: 12,
            error: None,
            screenshot: None,
            observed: None,
        };
        assert_eq!(
            serde_json::to_string(&step).unwrap(),
            r#"{"index":0,"op":"click","ok":true,"ms":12}"#
        );
        let receipt = RecipeResponse {
            ok: true,
            name: None,
            observe: None,
            ran: 1,
            stopped_at: None,
            duration_ms: 12,
            steps: vec![step],
            screenshot: None,
            artifacts: Vec::new(),
            recording_error: None,
        };
        assert_eq!(
            serde_json::to_string(&receipt).unwrap(),
            r#"{"ok":true,"ran":1,"duration_ms":12,"steps":[{"index":0,"op":"click","ok":true,"ms":12}]}"#
        );
    }

    /// A receipt is read a long way from the request. It has to say whether
    /// the box was looking, so that no `observed` block cannot be mistaken
    /// for nothing having been there to see.
    #[test]
    fn a_receipt_says_whether_the_box_was_looking() {
        let receipt = RecipeResponse {
            ok: true,
            name: None,
            observe: Some(RecipeObserve::Input),
            ran: 0,
            stopped_at: None,
            duration_ms: 0,
            steps: Vec::new(),
            screenshot: None,
            artifacts: Vec::new(),
            recording_error: None,
        };
        let json = serde_json::to_string(&receipt).unwrap();
        assert!(json.contains(r#""observe":"input""#), "{json}");
    }

    /// And `ok` keeps meaning exactly what it meant: no step returned an
    /// error. A step that was observed to have landed on nothing is still
    /// `ok: true`, because the box does not know what the recipe was for.
    #[test]
    fn observation_does_not_touch_ok() {
        let step = RecipeStepResult {
            index: 6,
            op: "type",
            ok: true,
            ms: 240,
            error: None,
            screenshot: None,
            observed: Some(crate::observe::StepObservation {
                focus: Some(crate::observe::Focus {
                    state: crate::observe::FocusState::None,
                    window: None,
                }),
                ..Default::default()
            }),
        };
        let json = serde_json::to_string(&step).unwrap();
        assert!(json.contains(r#""ok":true"#), "{json}");
        assert!(json.contains(r#""state":"none""#), "{json}");
    }

    #[test]
    fn accepts_known_path() {
        let req = RecipeRequest {
            name: Some("open-url".into()),
            stop_on_error: Some(true),
            screenshot: Some(RecipeScreenshot::End),
            steps: vec![
                RecipeStep::Click {
                    x: 640,
                    y: 400,
                    button: Some(1),
                },
                RecipeStep::Type {
                    text: "hello".into(),
                },
                RecipeStep::Key {
                    key: "Return".into(),
                    action: None,
                },
                RecipeStep::Wait { ms: 50 },
            ],
            ..RecipeRequest::default()
        };
        assert!(validate_recipe(&cfg(), &req).is_ok());
    }
}
