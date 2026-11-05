//! Smart waits while a recipe runs: Chromium mapped/focused, title or CDP URL
//! changed. Used by cook so raw v1 (and compressed v2/v3) do not type into a
//! window that is not on screen yet.

use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::Command;

use crate::CuaConfig;

const POLL_MS: u64 = 120;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WinInfo {
    pub id: String,
    pub class: String,
    pub title: String,
}

impl WinInfo {
    pub fn is_app(&self) -> bool {
        let class = self.class.to_ascii_lowercase();
        class.contains("chromium")
            || class.contains("chrome")
            || class.contains("xfce4-terminal")
            || class.contains("thunar")
    }

    pub fn is_chromium(&self) -> bool {
        let class = self.class.to_ascii_lowercase();
        class.contains("chromium") || class.contains("chrome")
    }

    pub fn usable(&self) -> bool {
        self.is_app() && !self.title.trim().is_empty()
    }
}

pub(crate) fn parse_wmctrl_lx(stdout: &str) -> Vec<WinInfo> {
    stdout.lines().filter_map(parse_wmctrl_lx_line).collect()
}

pub(crate) fn parse_wmctrl_lx_line(line: &str) -> Option<WinInfo> {
    let mut parts = line.split_whitespace();
    let id = parts.next()?.to_string();
    let _desktop = parts.next()?;
    let class = parts.next()?.to_string();
    let _host = parts.next()?;
    let title = parts.collect::<Vec<_>>().join(" ");
    if id.is_empty() {
        return None;
    }
    Some(WinInfo { id, class, title })
}

pub(crate) fn parse_cdp_page_url(body: &str) -> Option<String> {
    // Prefer a real http(s) page over the local newtab file.
    let mut first = None;
    let mut search = body;
    while let Some(idx) = search.find("\"url\"") {
        let rest = &search[idx + 5..];
        let Some(colon) = rest.find(':') else {
            break;
        };
        let after = rest[colon + 1..].trim_start();
        let quote = after.strip_prefix('"')?;
        let end = quote.find('"')?;
        let url = quote[..end].to_string();
        if first.is_none() {
            first = Some(url.clone());
        }
        if url.starts_with("http://") || url.starts_with("https://") {
            return Some(url);
        }
        search = &quote[end + 1..];
    }
    first
}

pub(crate) fn parse_cdp_page_title(body: &str) -> Option<String> {
    let idx = body.find("\"title\"")?;
    let rest = &body[idx + 7..];
    let colon = rest.find(':')?;
    let after = rest[colon + 1..].trim_start();
    let quote = after.strip_prefix('"')?;
    let end = quote.find('"')?;
    Some(quote[..end].to_string())
}

async fn wmctrl_lx(display: &str) -> Vec<WinInfo> {
    let out = Command::new("wmctrl")
        .env("DISPLAY", display)
        .arg("-lx")
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => parse_wmctrl_lx(&String::from_utf8_lossy(&o.stdout)),
        _ => Vec::new(),
    }
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

async fn show_window(display: &str, id: &str) {
    let _ = Command::new("xdotool")
        .env("DISPLAY", display)
        .args(["windowmap", id])
        .status()
        .await;
    let _ = Command::new("wmctrl")
        .env("DISPLAY", display)
        .args(unhide_wmctrl_args(id))
        .status()
        .await;
    let _ = Command::new("wmctrl")
        .env("DISPLAY", display)
        .args(["-ia", id])
        .status()
        .await;
    let _ = Command::new("xdotool")
        .env("DISPLAY", display)
        .args(["windowactivate", id])
        .status()
        .await;
    let _ = Command::new("xdotool")
        .env("DISPLAY", display)
        .args(["windowraise", id])
        .status()
        .await;
}

async fn window_viewable(display: &str, id: &str) -> bool {
    let out = Command::new("xwininfo")
        .env("DISPLAY", display)
        .args(["-id", id])
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => window_is_mapped(&String::from_utf8_lossy(&o.stdout)),
        _ => false,
    }
}

pub(crate) async fn cdp_json() -> Option<String> {
    let port: u16 = std::env::var("BOX_CDP_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(9222);
    let mut stream = tokio::time::timeout(
        Duration::from_millis(250),
        TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    .ok()?
    .ok()?;
    let req = b"GET /json HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
    stream.write_all(req).await.ok()?;
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_millis(400), async {
        let mut chunk = [0u8; 4096];
        while !box_common::http_response_complete(&buf) {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
    })
    .await
    .ok()?;
    let text = String::from_utf8_lossy(&buf);
    let split = text.find("\r\n\r\n").or_else(|| text.find("\n\n"))?;
    let start = if text[split..].starts_with("\r\n\r\n") {
        split + 4
    } else {
        split + 2
    };
    Some(text[start..].to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PageSnap {
    pub title: String,
    pub url: String,
}

pub(crate) async fn page_snap(config: &CuaConfig) -> PageSnap {
    let mut title = String::new();
    for win in wmctrl_lx(&config.display).await {
        if win.is_chromium() && !win.title.is_empty() {
            title = win.title;
            break;
        }
    }
    let body = cdp_json().await.unwrap_or_default();
    let url = parse_cdp_page_url(&body).unwrap_or_default();
    if title.is_empty() {
        title = parse_cdp_page_title(&body).unwrap_or_default();
    }
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
        let chromes: Vec<&WinInfo> = wins.iter().filter(|w| w.is_chromium()).collect();
        let mut visible_usable = false;
        for win in &chromes {
            if window_viewable(&config.display, &win.id).await {
                if win.usable() {
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
            if let Some(win) = wins.iter().find(|w| w.is_app()) {
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
        let win = parse_wmctrl_lx_line("0x02a00003  0 chromium.Chromium  box  New Tab - Chromium")
            .unwrap();
        assert_eq!(win.id, "0x02a00003");
        assert!(win.is_chromium());
        assert!(win.usable());
        assert_eq!(win.title, "New Tab - Chromium");
    }

    #[test]
    fn prefers_https_cdp_url() {
        let body = r#"[{"url":"file:///usr/share/buwiz-computer/newtab.html","title":"New Tab"},{"url":"https://www.facebook.com/login/","title":"Facebook"}]"#;
        assert_eq!(
            parse_cdp_page_url(body).as_deref(),
            Some("https://www.facebook.com/login/")
        );
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
