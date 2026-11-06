//! Virtual desktop — config and display probe for buwiz-computer.
//!
//! The X stack (Xvfb, xfce, one voxl per screen) is started by the container
//! entrypoint. This crate is a library used by `box-host` to advertise
//! `capabilities.desktop`, `GET /v1/desktop` and `GET /v1/screens`. It is not an
//! HTTP daemon.
//!
//! Computer Use coordinates follow `BOX_DISPLAY_GEOM` (default 1280×800).

use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use box_common::{env_bool, env_nonempty};
use serde::Serialize;

/// Capability flag name advertised by `box-host`.
pub const CAPABILITY: &str = "desktop";

/// Default X display (`BOX_DISPLAY`).
pub const PLANNED_DISPLAY: &str = ":1";

/// What serves a screen to a person: voxl, one process per screen.
pub const VIEWER_KIND: &str = "voxl";

/// A screen's viewer listens on this plus its display number: `:1` is 6901.
pub const VIEWER_PORT_BASE: u16 = 6900;

/// Default framebuffer width (CUA coordinate space).
pub const DISPLAY_WIDTH: u32 = 1280;

/// Default framebuffer height (CUA coordinate space).
pub const DISPLAY_HEIGHT: u32 = 800;

/// Default screen geometry for Xvfb (`WIDTHxHEIGHTxDEPTH`).
pub const DEFAULT_GEOMETRY: &str = "1280x800x24";

/// voxl's page. Sign in with `?token=<password>`; the URLs here never carry it.
pub const VIEWER_PATH: &str = "/";

/// Under the workspace: the box's own screen's folder. A Bot's is `agents/<bot>`.
pub const SHARED_WORKSPACE: &str = "shared";

/// Settings read from the environment and used by `box-host`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesktopConfig {
    /// When false, the entrypoint must not start X and probes always fail.
    pub enabled: bool,
    /// When true, `GET /v1/ready` requires a live display.
    pub required: bool,
    pub display: String,
    pub geometry: String,
}

impl DesktopConfig {
    pub fn from_env() -> Self {
        let enabled = env_bool("BOX_DESKTOP", true);
        let required = env_bool("BOX_DESKTOP_REQUIRED", enabled);
        Self {
            enabled,
            required,
            display: env_nonempty("BOX_DISPLAY").unwrap_or_else(|| PLANNED_DISPLAY.to_string()),
            geometry: env_nonempty("BOX_DISPLAY_GEOM")
                .unwrap_or_else(|| DEFAULT_GEOMETRY.to_string()),
        }
    }

    pub fn disabled() -> Self {
        Self {
            enabled: false,
            required: false,
            display: PLANNED_DISPLAY.to_string(),
            geometry: DEFAULT_GEOMETRY.to_string(),
        }
    }

    /// Unix domain socket Xvfb creates for this display (`:1` → `X1`).
    pub fn x11_socket_path(&self) -> PathBuf {
        x11_socket_path(&self.display)
    }

    /// True when desktop is enabled and the X display is accepting clients.
    pub fn probe_display(&self) -> bool {
        probe_display(self)
    }

    /// The port of this screen's viewer.
    pub fn viewer_port(&self) -> u16 {
        viewer_port(&self.display).unwrap_or(VIEWER_PORT_BASE + 1)
    }

    /// True when this screen's viewer is taking connections.
    pub fn viewer_ready(&self) -> bool {
        self.enabled && viewer_up(self.viewer_port())
    }

    pub fn viewer_url(&self, advertised_host: &str) -> String {
        let host = advertised_host.split(':').next().unwrap_or("127.0.0.1");
        format!("http://{host}:{}{VIEWER_PATH}", self.viewer_port())
    }

    pub fn pixel_size(&self) -> (u32, u32) {
        parse_geometry(&self.geometry).unwrap_or((DISPLAY_WIDTH, DISPLAY_HEIGHT))
    }
}

/// The display's number: `:1` and `:1.0` are 1.
pub fn display_number(display: &str) -> Option<u16> {
    let number = display.trim().strip_prefix(':')?.split('.').next()?;
    number.parse().ok()
}

/// The viewer port of a display: `:1` is 6901, a Bot's `:2` is 6902.
pub fn viewer_port(display: &str) -> Option<u16> {
    VIEWER_PORT_BASE.checked_add(display_number(display)?)
}

/// True when something takes connections on the box's own loopback at `port`.
pub fn viewer_up(port: u16) -> bool {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok()
}

/// Parse `WIDTHxHEIGHTxDEPTH` or `WIDTHxHEIGHT`.
pub fn parse_geometry(geom: &str) -> Option<(u32, u32)> {
    let mut parts = geom.split('x');
    let w = parts.next()?.parse().ok()?;
    let h = parts.next()?.parse().ok()?;
    if w == 0 || h == 0 {
        None
    } else {
        Some((w, h))
    }
}

/// Snapshot returned by `GET /v1/desktop`.
#[derive(Clone, Debug, Serialize)]
pub struct DesktopStatus {
    pub enabled: bool,
    pub ready: bool,
    /// Same as `ready`. Kept for existing clients / smoke scripts.
    pub available: bool,
    pub display: String,
    pub geometry: String,
    pub viewer: ViewerInfo,
}

#[derive(Clone, Debug, Serialize)]
pub struct ViewerInfo {
    pub kind: &'static str,
    pub port: u16,
    pub path: String,
    pub url: String,
}

impl DesktopStatus {
    pub fn from_config(config: &DesktopConfig, advertised_host: &str) -> Self {
        let ready = config.probe_display();
        Self {
            enabled: config.enabled,
            ready,
            available: ready,
            display: config.display.clone(),
            geometry: config.geometry.clone(),
            viewer: ViewerInfo {
                kind: VIEWER_KIND,
                port: config.viewer_port(),
                path: VIEWER_PATH.to_string(),
                url: config.viewer_url(advertised_host),
            },
        }
    }
}

/// Whether a screen can be used and watched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenState {
    /// The display is up and its viewer takes connections.
    Running,
    /// The display is up but its viewer is not (yet): input and screenshots work.
    ViewerDown,
    /// No display: a Bot's screen that `box-screen down` stopped, or a restart ended.
    Stopped,
}

/// One screen of the box, as `GET /v1/screens` lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Screen {
    /// `:1` is the box's own; a Bot's own screen is `:2` and up.
    pub display: String,
    /// The display's number: the N of `6900+N`.
    pub slot: u16,
    /// The Bot this screen belongs to. `None` on the box's own screen.
    pub bot: Option<String>,
    pub viewer_port: u16,
    pub state: ScreenState,
    /// What this screen's agent makes goes here, relative to the workspace root, so it can be
    /// collected per agent.
    pub workspace: String,
}

/// Snapshot returned by `GET /v1/screens`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ScreenList {
    pub screens: Vec<Screen>,
}

impl ScreenList {
    /// The box's own screen, then every Bot's, by display number. Empty with the desktop off.
    pub fn from_config(config: &DesktopConfig, slots_dir: &Path) -> Self {
        let mut screens = Vec::new();
        if !config.enabled {
            return Self { screens };
        }
        let slot = display_number(&config.display).unwrap_or(1);
        screens.push(screen(slot, None));
        let mut bots = read_slots(slots_dir);
        bots.sort_by_key(|(_, slot)| *slot);
        screens.extend(bots.into_iter().map(|(bot, n)| screen(n, Some(bot))));
        Self { screens }
    }
}

/// Where `box-screen` keeps a Bot's slot: one file per Bot, its display number inside. Root's.
pub fn slots_dir() -> PathBuf {
    PathBuf::from("/home/box/.box/slots")
}

/// The folder of a screen, relative to the workspace root: `agents/<bot>` for a Bot's own,
/// [`SHARED_WORKSPACE`] for the box's own or a display no Bot holds.
pub fn screen_workspace(display: &str, slots_dir: &Path) -> String {
    let bot = display_number(display).and_then(|slot| {
        read_slots(slots_dir)
            .into_iter()
            .find(|(_, held)| *held == slot)
            .map(|(bot, _)| bot)
    });
    workspace_of(bot.as_deref())
}

/// The Bot holding this slot.
pub fn slot_holder(slots_dir: &Path, slot: u16) -> Option<String> {
    read_slots(slots_dir)
        .into_iter()
        .find(|(_, held)| *held == slot)
        .map(|(bot, _)| bot)
}

/// True when `box-screen` gave this slot to a Bot.
pub fn slot_is_allocated(slots_dir: &Path, slot: u16) -> bool {
    read_slots(slots_dir).iter().any(|(_, held)| *held == slot)
}

fn workspace_of(bot: Option<&str>) -> String {
    bot.map_or_else(
        || SHARED_WORKSPACE.to_string(),
        |bot| format!("agents/{bot}"),
    )
}

fn read_slots(dir: &Path) -> Vec<(String, u16)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let bot = entry.file_name().into_string().ok()?;
            let slot = std::fs::read_to_string(entry.path()).ok()?;
            Some((bot, slot.trim().parse().ok()?))
        })
        .collect()
}

fn screen(slot: u16, bot: Option<String>) -> Screen {
    let display = format!(":{slot}");
    let viewer_port = VIEWER_PORT_BASE.saturating_add(slot);
    Screen {
        state: screen_state(
            UnixStream::connect(x11_socket_path(&display)).is_ok(),
            viewer_up(viewer_port),
        ),
        workspace: workspace_of(bot.as_deref()),
        display,
        slot,
        bot,
        viewer_port,
    }
}

fn screen_state(display_up: bool, viewer_up: bool) -> ScreenState {
    match (display_up, viewer_up) {
        (false, _) => ScreenState::Stopped,
        (true, false) => ScreenState::ViewerDown,
        (true, true) => ScreenState::Running,
    }
}

/// One mapped (or listed) X window from `wmctrl -lx`.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct DesktopWindow {
    pub id: String,
    pub desktop: i32,
    pub class: String,
    pub title: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct WindowList {
    pub display: String,
    pub available: bool,
    pub windows: Vec<DesktopWindow>,
}

impl WindowList {
    pub fn from_config(config: &DesktopConfig) -> Self {
        if !config.enabled || !config.probe_display() {
            return Self {
                display: config.display.clone(),
                available: false,
                windows: Vec::new(),
            };
        }
        Self {
            display: config.display.clone(),
            available: true,
            windows: list_windows(&config.display),
        }
    }
}

/// `wmctrl -lx` on `DISPLAY`. Empty if wmctrl is missing.
pub fn list_windows(display: &str) -> Vec<DesktopWindow> {
    let out = Command::new("wmctrl")
        .env("DISPLAY", display)
        .arg("-lx")
        .output();
    match out {
        Ok(o) if o.status.success() => parse_wmctrl_lx(&String::from_utf8_lossy(&o.stdout)),
        _ => Vec::new(),
    }
}

pub fn parse_wmctrl_lx(stdout: &str) -> Vec<DesktopWindow> {
    stdout.lines().filter_map(parse_wmctrl_lx_line).collect()
}

pub fn parse_wmctrl_lx_line(line: &str) -> Option<DesktopWindow> {
    let mut parts = line.split_whitespace();
    let id = parts.next()?.to_string();
    let desktop = parts.next()?.parse().unwrap_or(-1);
    let class = parts.next()?.to_string();
    let _host = parts.next()?;
    let title = parts.collect::<Vec<_>>().join(" ");
    if id.is_empty() {
        return None;
    }
    Some(DesktopWindow {
        id,
        desktop,
        class,
        title,
    })
}

pub fn probe_display(config: &DesktopConfig) -> bool {
    if !config.enabled {
        return false;
    }
    let socket = config.x11_socket_path();
    if !socket.exists() {
        return false;
    }
    match Command::new("xdpyinfo")
        .arg("-display")
        .arg(&config.display)
        .output()
    {
        Ok(out) => out.status.success(),
        Err(_) => true,
    }
}

fn x11_socket_path(display: &str) -> PathBuf {
    let num = display
        .trim()
        .trim_start_matches(':')
        .split('.')
        .next()
        .unwrap_or("1");
    Path::new("/tmp/.X11-unix").join(format!("X{num}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_from_display() {
        assert_eq!(x11_socket_path(":1"), PathBuf::from("/tmp/.X11-unix/X1"));
        assert_eq!(x11_socket_path(":1.0"), PathBuf::from("/tmp/.X11-unix/X1"));
        assert_eq!(x11_socket_path("2"), PathBuf::from("/tmp/.X11-unix/X2"));
    }

    #[test]
    fn disabled_never_available() {
        let cfg = DesktopConfig::disabled();
        assert!(!cfg.enabled);
        assert!(!cfg.probe_display());
        let status = DesktopStatus::from_config(&cfg, "127.0.0.1:1340");
        assert!(!status.available);
        assert!(!status.enabled);
        assert!(!status.ready);
        assert_eq!(status.viewer.kind, "voxl");
        assert_eq!(status.viewer.port, 6901);
        assert_eq!(status.viewer.path, "/");
        assert_eq!(status.viewer.url, "http://127.0.0.1:6901/");
    }

    #[test]
    fn viewer_url_strips_port_from_host() {
        let cfg = DesktopConfig::disabled();
        assert_eq!(cfg.viewer_url("0.0.0.0:1340"), "http://0.0.0.0:6901/");
    }

    #[test]
    fn viewer_port_follows_the_display() {
        assert_eq!(viewer_port(":1"), Some(6901));
        assert_eq!(viewer_port(":1.0"), Some(6901));
        assert_eq!(viewer_port(":5"), Some(6905));
        assert_eq!(viewer_port("1"), None);
        assert_eq!(viewer_port(":x"), None);
    }

    #[test]
    fn screen_state_needs_the_display_first() {
        assert_eq!(screen_state(true, true), ScreenState::Running);
        assert_eq!(screen_state(true, false), ScreenState::ViewerDown);
        assert_eq!(screen_state(false, true), ScreenState::Stopped);
        assert_eq!(screen_state(false, false), ScreenState::Stopped);
    }

    #[test]
    fn no_screens_with_the_desktop_off() {
        let list = ScreenList::from_config(&DesktopConfig::disabled(), Path::new("/nowhere"));
        assert!(list.screens.is_empty());
    }

    #[test]
    fn lists_the_box_screen_then_each_bot_by_slot() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("zed"), "2\n").unwrap();
        std::fs::write(dir.path().join("amy"), "3\n").unwrap();
        std::fs::write(dir.path().join("broken"), "not a number").unwrap();
        let config = DesktopConfig {
            enabled: true,
            ..DesktopConfig::disabled()
        };
        let list = ScreenList::from_config(&config, dir.path());
        let shape: Vec<_> = list
            .screens
            .iter()
            .map(|s| (s.display.as_str(), s.bot.as_deref(), s.viewer_port))
            .collect();
        assert_eq!(
            shape,
            [
                (":1", None, 6901),
                (":2", Some("zed"), 6902),
                (":3", Some("amy"), 6903)
            ]
        );
    }

    #[test]
    fn screen_serializes_the_contract_names() {
        let body = serde_json::to_value(Screen {
            display: ":2".into(),
            slot: 2,
            bot: Some("amy".into()),
            viewer_port: 6902,
            state: ScreenState::ViewerDown,
            workspace: "agents/amy".into(),
        })
        .unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "display": ":2",
                "slot": 2,
                "bot": "amy",
                "viewer_port": 6902,
                "state": "viewer_down",
                "workspace": "agents/amy",
            })
        );
    }

    #[test]
    fn a_screen_folder_is_its_bots_or_the_shared_one() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("amy"), "3\n").unwrap();
        assert_eq!(screen_workspace(":3", dir.path()), "agents/amy");
        assert_eq!(screen_workspace(":1", dir.path()), "shared");
        assert_eq!(screen_workspace(":4", dir.path()), "shared");
    }

    #[test]
    fn parse_geometry_1280x800() {
        assert_eq!(parse_geometry("1280x800x24"), Some((1280, 800)));
        assert_eq!(parse_geometry("1280x800"), Some((1280, 800)));
        assert_eq!(parse_geometry("bad"), None);
    }

    #[test]
    fn parses_wmctrl_line() {
        let win = parse_wmctrl_lx_line("0x02a00003  0 chromium.Chromium  box  New Tab - Chromium")
            .unwrap();
        assert_eq!(win.id, "0x02a00003");
        assert_eq!(win.desktop, 0);
        assert_eq!(win.class, "chromium.Chromium");
        assert_eq!(win.title, "New Tab - Chromium");
    }
}
