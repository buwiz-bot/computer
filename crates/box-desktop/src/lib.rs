//! Virtual desktop — config and display probe for buwiz-computer.
//!
//! The X stack (Xvfb, openbox, x11vnc, noVNC) is started by the container
//! entrypoint. This crate is a library used by `box-host` to advertise
//! `capabilities.desktop` and `GET /v1/desktop`. It is not an HTTP daemon.
//!
//! Computer Use coordinates follow `BOX_DISPLAY_GEOM` (default 1280×800).

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use box_common::{env_bool, env_nonempty};
use serde::Serialize;

/// Capability flag name advertised by `box-host`.
pub const CAPABILITY: &str = "desktop";

/// Default X display (`BOX_DISPLAY`).
pub const PLANNED_DISPLAY: &str = ":1";

/// Default noVNC / viewer port (not the VNC RFB port).
pub const PLANNED_VIEWER_PORT: u16 = 6080;

/// Default framebuffer width (CUA coordinate space).
pub const DISPLAY_WIDTH: u32 = 1280;

/// Default framebuffer height (CUA coordinate space).
pub const DISPLAY_HEIGHT: u32 = 800;

/// Default screen geometry for Xvfb (`WIDTHxHEIGHTxDEPTH`).
pub const DEFAULT_GEOMETRY: &str = "1280x800x24";

/// Default VNC bind (localhost only).
pub const DEFAULT_VNC_BIND: &str = "127.0.0.1:5900";

/// noVNC entry HTML path.
pub const VIEWER_PATH: &str = "/vnc.html";

/// Settings read from the environment and used by `box-host`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesktopConfig {
    /// When false, the entrypoint must not start X and probes always fail.
    pub enabled: bool,
    /// When true, `GET /v1/ready` requires a live display.
    pub required: bool,
    pub display: String,
    pub geometry: String,
    pub vnc_bind: String,
    pub novnc_port: u16,
    pub viewer_path: String,
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
            vnc_bind: env_nonempty("BOX_VNC_BIND").unwrap_or_else(|| DEFAULT_VNC_BIND.to_string()),
            novnc_port: env::var("BOX_NOVNC_PORT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(PLANNED_VIEWER_PORT),
            viewer_path: VIEWER_PATH.to_string(),
        }
    }

    pub fn disabled() -> Self {
        Self {
            enabled: false,
            required: false,
            display: PLANNED_DISPLAY.to_string(),
            geometry: DEFAULT_GEOMETRY.to_string(),
            vnc_bind: DEFAULT_VNC_BIND.to_string(),
            novnc_port: PLANNED_VIEWER_PORT,
            viewer_path: VIEWER_PATH.to_string(),
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

    pub fn viewer_url(&self, advertised_host: &str) -> String {
        let host = advertised_host.split(':').next().unwrap_or("127.0.0.1");
        format!("http://{host}:{}{}", self.novnc_port, self.viewer_path)
    }

    pub fn pixel_size(&self) -> (u32, u32) {
        parse_geometry(&self.geometry).unwrap_or((DISPLAY_WIDTH, DISPLAY_HEIGHT))
    }
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
    pub vnc: String,
    pub viewer: ViewerInfo,
}

#[derive(Clone, Debug, Serialize)]
pub struct ViewerInfo {
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
            vnc: config.vnc_bind.clone(),
            viewer: ViewerInfo {
                port: config.novnc_port,
                path: config.viewer_path.clone(),
                url: config.viewer_url(advertised_host),
            },
        }
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
        assert_eq!(status.viewer.port, 6080);
        assert_eq!(status.viewer.path, "/vnc.html");
        assert_eq!(status.viewer.url, "http://127.0.0.1:6080/vnc.html");
    }

    #[test]
    fn viewer_url_strips_port_from_host() {
        let cfg = DesktopConfig::disabled();
        assert_eq!(
            cfg.viewer_url("0.0.0.0:1340"),
            "http://0.0.0.0:6080/vnc.html"
        );
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
