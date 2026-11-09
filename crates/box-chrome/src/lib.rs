//! Chromium launch metadata for the Linux Buwiz computer image.
//!
//! Chrome (or Chromium) is started by `docker/entrypoint.sh` on `DISPLAY=:1`.
//! The profile lives at `/home/box/desktops/main/chrome-profile` (compose volume).
//!
//! It has no DevTools port. One on loopback has no login, and every user in the box shares that
//! loopback, so any command could read the browser's cookies and drive it. Readiness is the
//! process the entrypoint recorded, in a folder only its own user writes.
//!
//! It has a DevTools pipe instead, whose other end only `box-exec` holds: [`pipe`].

pub mod pipe;

use box_common::env_bool;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Default Chromium `--user-data-dir`.
pub const DEFAULT_PROFILE: &str = "/home/box/desktops/main/chrome-profile";

pub const PID_FILE: &str = "/run/box/x/1/run/chrome.pid";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChromeConfig {
    pub enabled: bool,
    pub profile: PathBuf,
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
        let display = std::env::var("BOX_DISPLAY").unwrap_or_else(|_| ":1".into());
        Self {
            enabled,
            profile,
            display,
        }
    }

    pub fn disabled() -> Self {
        Self {
            enabled: false,
            profile: PathBuf::from(DEFAULT_PROFILE),
            display: ":1".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChromeStatus {
    pub enabled: bool,
    /// The entrypoint's Chromium is running.
    pub ready: bool,
    /// The same: the entrypoint pidfile points at a live pid.
    pub running: bool,
    pub profile: String,
    pub display: String,
}

pub fn probe_chrome(cfg: &ChromeConfig) -> ChromeStatus {
    if !cfg.enabled {
        return ChromeStatus {
            enabled: false,
            ready: false,
            running: false,
            profile: cfg.profile.display().to_string(),
            display: cfg.display.clone(),
        };
    }
    let running = pidfile_alive(Path::new(PID_FILE));
    ChromeStatus {
        enabled: true,
        ready: running,
        running,
        profile: cfg.profile.display().to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_is_not_running() {
        let st = probe_chrome(&ChromeConfig::disabled());
        assert!(!st.enabled);
        assert!(!st.running);
        assert!(!st.ready);
    }
}
