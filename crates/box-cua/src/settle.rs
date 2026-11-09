//! Smart waits while a recipe runs: Chromium mapped/focused, title or CDP URL
//! changed. Used by cook so raw v1 (and compressed v2/v3) do not type into a
//! window that is not on screen yet.

use std::time::{Duration, Instant};

use box_desktop::{parse_wmctrl_lx, DesktopWindow};
use tokio::process::Command;

use crate::xdotool::{run_tool, WINDOW_TOOL_TIMEOUT};
use crate::CuaConfig;

const POLL_MS: u64 = 120;

fn is_app(win: &DesktopWindow) -> bool {
    let class = win.class.to_ascii_lowercase();
    class.contains("chromium")
        || class.contains("chrome")
        || class.contains("xfce4-terminal")
        || class.contains("thunar")
}

fn is_chromium(win: &DesktopWindow) -> bool {
    let class = win.class.to_ascii_lowercase();
    class.contains("chromium") || class.contains("chrome")
}

fn usable(win: &DesktopWindow) -> bool {
    is_app(win) && !win.title.trim().is_empty()
}

/// Run a window tool on `display`. `None` when it could not run, ran out of time, or exited
/// non-zero: in each case it said nothing.
async fn window_tool(display: &str, program: &str, args: &[&str]) -> Option<Vec<u8>> {
    let out = run_tool(
        Command::new(program).env("DISPLAY", display).args(args),
        WINDOW_TOOL_TIMEOUT,
    )
    .await;
    match out {
        Ok(out) if out.status.success() => Some(out.stdout),
        Ok(out) => {
            tracing::debug!(
                program,
                args = args.join(" "),
                status = %out.status,
                "window tool failed"
            );
            None
        }
        Err(err) => {
            tracing::debug!(program, args = args.join(" "), error = %err, "window tool failed");
            None
        }
    }
}

async fn wmctrl_lx(display: &str) -> Vec<DesktopWindow> {
    window_tool(display, "wmctrl", &["-lx"])
        .await
        .map(|stdout| parse_wmctrl_lx(&String::from_utf8_lossy(&stdout)))
        .unwrap_or_default()
}

/// `wmctrl -ia` does not map an Iconic / `_NET_WM_STATE_HIDDEN` window.
/// Dock launchers toggle hide; cook then typed into an unmapped Chromium
/// while ffmpeg/x11grab correctly recorded the wallpaper.
pub(crate) fn window_is_mapped(xwininfo: &str) -> bool {
    xwininfo.contains("IsViewable")
}

pub(crate) fn unhide_wmctrl_args(id: &str) -> [&str; 4] {
    ["-i", "-b", "remove,hidden", id]
}

/// Best effort, step by step: a step that fails is logged and the next one still runs, and
/// whether it worked is read back by [`window_viewable`].
async fn show_window(display: &str, id: &str) {
    window_tool(display, "xdotool", &["windowmap", id]).await;
    window_tool(display, "wmctrl", &unhide_wmctrl_args(id)).await;
    window_tool(display, "wmctrl", &["-ia", id]).await;
    window_tool(display, "xdotool", &["windowactivate", id]).await;
    window_tool(display, "xdotool", &["windowraise", id]).await;
}

async fn window_viewable(display: &str, id: &str) -> bool {
    window_tool(display, "xwininfo", &["-id", id])
        .await
        .is_some_and(|stdout| window_is_mapped(&String::from_utf8_lossy(&stdout)))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PageSnap {
    pub title: String,
    pub url: String,
}

pub(crate) async fn page_snap(config: &CuaConfig) -> PageSnap {
    let mut title = String::new();
    for win in wmctrl_lx(&config.display).await {
        if is_chromium(&win) && !win.title.is_empty() {
            title = win.title;
            break;
        }
    }
    // this screen's own browser's report, and not the main screen's
    let url = box_desktop::page_url(&config.display).unwrap_or_default();
    PageSnap { title, url }
}

fn page_ready(snap: &PageSnap) -> bool {
    let url = snap.url.to_ascii_lowercase();
    let title = snap.title.to_ascii_lowercase();
    if url.starts_with("http://") || url.starts_with("https://") {
        return !url.contains("newtab.html");
    }
    if title.is_empty() || title.starts_with("new tab") {
        return false;
    }
    !title.contains("newtab.html")
}

fn page_changed(before: &PageSnap, now: &PageSnap) -> bool {
    if !now.url.is_empty() && now.url != before.url {
        return true;
    }
    if !now.title.is_empty() && now.title != before.title {
        return true;
    }
    page_ready(now) && !page_ready(before)
}

/// Wait for an existing Chromium window. Never launches `box-chromium`.
pub(crate) async fn wait_chromium_usable(config: &CuaConfig, timeout_ms: u64) -> bool {
    let started = Instant::now();
    let deadline = started + Duration::from_millis(timeout_ms);
    loop {
        let wins = wmctrl_lx(&config.display).await;
        let chromes: Vec<&DesktopWindow> = wins.iter().filter(|w| is_chromium(w)).collect();
        let mut visible_usable = false;
        for win in &chromes {
            if window_viewable(&config.display, &win.id).await {
                if usable(win) {
                    visible_usable = true;
                }
            } else {
                show_window(&config.display, &win.id).await;
            }
        }
        if visible_usable {
            tokio::time::sleep(Duration::from_millis(250)).await;
            return true;
        }
        if Instant::now() >= deadline {
            if let Some(win) = wins.iter().find(|w| is_app(w)) {
                show_window(&config.display, &win.id).await;
            }
            return false;
        }
        tokio::time::sleep(Duration::from_millis(POLL_MS)).await;
    }
}

pub(crate) async fn wait_page_change(
    config: &CuaConfig,
    before: &PageSnap,
    timeout_ms: u64,
) -> bool {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let now = page_snap(config).await;
        if page_changed(before, &now) {
            tokio::time::sleep(Duration::from_millis(350)).await;
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(POLL_MS)).await;
    }
}

pub(crate) fn dock_click(y: i32, height: u32) -> bool {
    let band = i32::try_from(height.saturating_sub(90)).unwrap_or(710);
    y >= band
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wmctrl_chromium_line() {
        let win = box_desktop::parse_wmctrl_lx_line(
            "0x02a00003  0 chromium.Chromium  box  New Tab - Chromium",
        )
        .unwrap();
        assert_eq!(win.id, "0x02a00003");
        assert!(is_chromium(&win));
        assert!(usable(&win));
        assert_eq!(win.title, "New Tab - Chromium");
    }

    #[test]
    fn dock_band_matches_tint2() {
        assert!(dock_click(764, 800));
        assert!(!dock_click(159, 800));
    }

    #[test]
    fn unhide_uses_remove_hidden_not_only_activate() {
        assert_eq!(
            unhide_wmctrl_args("0x00600003"),
            ["-i", "-b", "remove,hidden", "0x00600003"]
        );
    }

    #[test]
    fn mapped_means_viewable_not_iconic() {
        assert!(window_is_mapped("  Map State: IsViewable\n  Width: 1260\n"));
        assert!(!window_is_mapped(
            "  Map State: IsUnMapped\n  Width: 1260\n"
        ));
    }
}
