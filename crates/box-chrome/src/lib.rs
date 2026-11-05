//! Chromium launch metadata for the Linux Buwiz computer image.
//!
//! Chrome (or Chromium) is started by `docker/entrypoint.sh` on `DISPLAY=:1`.
//! The profile lives at `/home/box/chrome-profile` (compose volume). Optional
//! Chrome DevTools Protocol listens on **localhost only**.
//!
//! Readiness is CDP (`GET /json/version` on loopback). We do not walk `/proc`
//! looking for chrome binaries.

use box_common::env_bool;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Default Chromium `--user-data-dir`.
pub const DEFAULT_PROFILE: &str = "/home/box/chrome-profile";

/// CDP is never published; agents inside the box talk to this loopback port.
pub const DEFAULT_CDP_PORT: u16 = 9222;

pub const PID_FILE: &str = "/tmp/box-chrome.pid";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChromeConfig {
    pub enabled: bool,
    pub profile: PathBuf,
    pub cdp_port: u16,
    pub display: String,
}

impl ChromeConfig {
    pub fn from_env() -> Self {
        // Chromium needs a screen: with the desktop off the entrypoint never starts it.
        let enabled = env_bool("BOX_CHROME", true) && env_bool("BOX_DESKTOP", true);
        let profile = std::env::var("BOX_CHROME_PROFILE")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_PROFILE));
        let cdp_port = std::env::var("BOX_CDP_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_CDP_PORT);
        let display = std::env::var("BOX_DISPLAY").unwrap_or_else(|_| ":1".into());
        Self {
            enabled,
            profile,
            cdp_port,
            display,
        }
    }

    pub fn disabled() -> Self {
        Self {
            enabled: false,
            profile: PathBuf::from(DEFAULT_PROFILE),
            cdp_port: DEFAULT_CDP_PORT,
            display: ":1".into(),
        }
    }

    pub fn cdp_bind(&self) -> String {
        format!("127.0.0.1:{}", self.cdp_port)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChromeStatus {
    pub enabled: bool,
    /// CDP answered `/json/version` (honest "Chromium is usable").
    pub ready: bool,
    /// CDP ready, or the entrypoint pidfile still points at a live pid.
    pub running: bool,
    pub profile: String,
    pub cdp: Option<String>,
    pub display: String,
}

pub fn probe_chrome(cfg: &ChromeConfig) -> ChromeStatus {
    if !cfg.enabled {
        return ChromeStatus {
            enabled: false,
            ready: false,
            running: false,
            profile: cfg.profile.display().to_string(),
            cdp: None,
            display: cfg.display.clone(),
        };
    }
    let ready = cdp_version_ok(cfg.cdp_port);
    let running = ready || pidfile_alive(Path::new(PID_FILE));
    ChromeStatus {
        enabled: true,
        ready,
        running,
        profile: cfg.profile.display().to_string(),
        cdp: Some(cfg.cdp_bind()),
        display: cfg.display.clone(),
    }
}

fn pidfile_alive(path: &Path) -> bool {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(pid) = raw.trim().parse::<u32>() else {
        return false;
    };
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Chromium CDP `GET /json/version` on loopback. Not a generic TCP ping:
/// something else bound to 9222 must not count as chrome-ready.
pub fn cdp_version_ok(port: u16) -> bool {
    cdp_get(port, "/json/version").is_some_and(|body| {
        let lower = body.to_ascii_lowercase();
        lower.contains("browser")
            || lower.contains("chromium")
            || lower.contains("chrome")
            || lower.contains("web-socket")
            || lower.contains("websocket")
            || lower.contains("protocol-version")
    })
}

fn cdp_get(port: u16, path: &str) -> Option<String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(200)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(400)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_millis(200)))
        .ok()?;
    let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while !box_common::http_response_complete(&buf) {
        match stream.read(&mut chunk).ok()? {
            0 => break,
            n => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let text = String::from_utf8_lossy(&buf);
    if !(text.starts_with("HTTP/1.1 200") || text.starts_with("HTTP/1.0 200")) {
        return None;
    }
    let split = text.find("\r\n\r\n").or_else(|| text.find("\n\n"))?;
    let start = if text[split..].starts_with("\r\n\r\n") {
        split + 4
    } else {
        split + 2
    };
    Some(text[start..].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_is_not_running() {
        let st = probe_chrome(&ChromeConfig::disabled());
        assert!(!st.enabled);
        assert!(!st.running);
        assert!(!st.ready);
        assert!(st.cdp.is_none());
    }

    #[test]
    fn cdp_is_loopback_only() {
        let cfg = ChromeConfig::disabled();
        assert!(cfg.cdp_bind().starts_with("127.0.0.1:"));
    }

    #[test]
    fn closed_port_is_not_cdp_ready() {
        // 1 is TCP discard on some systems; 59999 is almost certainly closed here.
        assert!(!cdp_version_ok(59999));
    }
}
