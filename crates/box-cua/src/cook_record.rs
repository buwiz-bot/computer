//! ffmpeg x11grab of the guest framebuffer during a recipe cook.
//!
//! Starts **before** the first CUA step and is stopped with SIGINT **after**
//! the last step so the MP4 is finalized. Do not pass `-t`: that is what made
//! tapes die at 1s. Live capture uses fragmented MP4 (`frag_keyframe+empty_moov`)
//! so SIGINT can finish the file. `+faststart` is not used on that pass — it
//! needs a second pass and leaves a 1s-looking file when ffmpeg is interrupted.
//! After SIGINT, remux with `-c copy -movflags +faststart`. Finder / QuickTime
//! and Chrome treat `iso5` empty-moov x264 as a still (Play does not change
//! pixels; duration is often wrong, e.g. 16s for a 12s tape) even when the
//! file has many unique frames.
//!
//! Exact capture command (DISPLAY `:1`, 1280×800 → `:1.0`):
//!
//! ```text
//! ffmpeg -nostdin -hide_banner -loglevel error -y \
//!   -f x11grab -draw_mouse 1 -video_size 1280x800 -framerate 8 -i :1.0 \
//!   -an -c:v libx264 -preset ultrafast -pix_fmt yuv420p \
//!   -b:v 700k -maxrate 900k -bufsize 1800k \
//!   -movflags +frag_keyframe+empty_moov+default_base_moof \
//!   /workspace/.l1/cooks/<run>/cook.mp4
//! ```
//!
//! Then remux (stream copy, no re-encode):
//!
//! ```text
//! ffmpeg -nostdin -hide_banner -loglevel error -y \
//!   -i /workspace/.l1/cooks/<run>/cook.mp4 \
//!   -an -c:v copy -movflags +faststart \
//!   /workspace/.l1/cooks/<run>/cook.faststart.mp4
//! ```

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::process::{Child, Command};

use crate::CuaError;

pub struct CookRecorder {
    child: Child,
    pub path: PathBuf,
}

pub(crate) fn display_target(display: &str) -> String {
    if display.contains('.') {
        display.to_string()
    } else {
        format!("{display}.0")
    }
}

/// Argv after `ffmpeg` for a cook tape. No `-t` (duration cap).
pub fn cook_ffmpeg_args(size: &str, target: &str, dest: &str) -> Vec<String> {
    [
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-f",
        "x11grab",
        "-draw_mouse",
        "1",
        "-video_size",
        size,
        "-framerate",
        "8",
        "-i",
        target,
        "-an",
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-pix_fmt",
        "yuv420p",
        "-b:v",
        "700k",
        "-maxrate",
        "900k",
        "-bufsize",
        "1800k",
        "-movflags",
        "+frag_keyframe+empty_moov+default_base_moof",
        dest,
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

/// Argv after `ffmpeg` to turn a fragmented cook tape into progressive +faststart.
pub fn cook_remux_args(src: &str, dest: &str) -> Vec<String> {
    [
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-i",
        src,
        "-an",
        "-c:v",
        "copy",
        "-movflags",
        "+faststart",
        dest,
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn remux_temp_path(path: &Path) -> PathBuf {
    path.with_extension("faststart.mp4")
}

async fn remux_cook_mp4(path: &Path) -> Result<(), CuaError> {
    let src = path
        .to_str()
        .ok_or_else(|| CuaError::Invalid("cook remux path is not utf-8".into()))?
        .to_string();
    let tmp = remux_temp_path(path);
    if tmp == path {
        return Err(CuaError::Invalid("cook remux temp path collided".into()));
    }
    let dest = tmp
        .to_str()
        .ok_or_else(|| CuaError::Invalid("cook remux temp path is not utf-8".into()))?
        .to_string();
    let args = cook_remux_args(&src, &dest);
    let output = Command::new("ffmpeg")
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|err| CuaError::Tool(format!("ffmpeg remux could not start ({err})")))?;
    if !output.status.success() {
        let _ = tokio::fs::remove_file(&tmp).await;
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(CuaError::Tool(format!(
            "ffmpeg remux failed ({status}): {detail}",
            status = output.status
        )));
    }
    let meta = tokio::fs::metadata(&tmp)
        .await
        .map_err(|err| CuaError::Tool(format!("cook remux output missing: {err}")))?;
    if meta.len() == 0 {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(CuaError::Tool("cook remux output is empty".into()));
    }
    tokio::fs::rename(&tmp, path)
        .await
        .map_err(|err| CuaError::Tool(format!("cook remux replace: {err}")))?;
    Ok(())
}

async fn try_wait_now(child: &mut Child) -> Option<std::process::ExitStatus> {
    child.try_wait().ok().flatten()
}

async fn read_child_stderr(child: &mut Child) -> String {
    let mut detail = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf).await;
        detail = String::from_utf8_lossy(&buf).trim().to_string();
    }
    detail
}

pub async fn start_cook_recorder(
    display: &str,
    width: u32,
    height: u32,
    dest: PathBuf,
) -> Result<CookRecorder, CuaError> {
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|err| CuaError::Tool(format!("cook record dir: {err}")))?;
    }
    if dest.exists() {
        let _ = tokio::fs::remove_file(&dest).await;
    }

    let size = format!("{width}x{height}");
    let target = display_target(display);
    let dest_str = dest
        .to_str()
        .ok_or_else(|| CuaError::Invalid("cook record path is not utf-8".into()))?
        .to_string();
    let args = cook_ffmpeg_args(&size, &target, &dest_str);
    let grab_display = display;
    tracing::info!(
        grab_display,
        target = target.as_str(),
        dest = dest_str.as_str(),
        "ffmpeg cook recording start"
    );

    let mut child = Command::new("ffmpeg")
        .args(&args)
        .env("DISPLAY", display)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| {
            CuaError::Tool(format!(
                "ffmpeg could not start cook recording ({err}). Rebuild buwiz-computer:local with ffmpeg."
            ))
        })?;

    // Confirm the process is still up, then wait until the file has bytes so
    // step 0 is not a black leader.
    let deadline = Instant::now() + Duration::from_millis(800);
    loop {
        tokio::time::sleep(Duration::from_millis(80)).await;
        if let Some(status) = try_wait_now(&mut child).await {
            let detail = read_child_stderr(&mut child).await;
            return Err(CuaError::Tool(format!(
                "ffmpeg cook recording exited immediately ({status}): {detail}"
            )));
        }
        if tokio::fs::metadata(&dest)
            .await
            .map(|m| m.len() > 0)
            .unwrap_or(false)
        {
            break;
        }
        if Instant::now() >= deadline {
            // x11grab is running; first fragment may still be in the muxer.
            break;
        }
    }

    Ok(CookRecorder { child, path: dest })
}

pub async fn stop_cook_recorder(mut rec: CookRecorder) -> Result<PathBuf, CuaError> {
    let pid = rec.child.id();
    tracing::info!(path = %rec.path.display(), pid, "ffmpeg cook recording stop");
    if let Some(pid) = pid {
        let _ = Command::new("kill")
            .args(["-INT", &pid.to_string()])
            .status()
            .await;
    }
    match tokio::time::timeout(Duration::from_secs(8), rec.child.wait()).await {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => {
            tracing::warn!(error = %err, "ffmpeg wait failed after SIGINT");
        }
        Err(_) => {
            let _ = rec.child.start_kill();
            let _ = rec.child.wait().await;
        }
    }

    let meta = tokio::fs::metadata(&rec.path).await.map_err(|err| {
        CuaError::Tool(format!(
            "cook recording was not written to {}: {err}",
            rec.path.display()
        ))
    })?;
    if meta.len() == 0 {
        return Err(CuaError::Tool("cook recording file is empty".into()));
    }
    if let Err(err) = remux_cook_mp4(&rec.path).await {
        // Keep the fragmented tape so the receipt still has a file.
        tracing::warn!(
            error = %err,
            path = %rec.path.display(),
            "cook remux to +faststart failed; leaving fragmented mp4"
        );
    }
    Ok(rec.path)
}

pub fn recording_mime(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "webm" => "video/webm",
        _ => "video/mp4",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffmpeg_command_has_no_duration_cap() {
        let args = cook_ffmpeg_args("1280x800", ":1.0", "/workspace/.l1/cooks/demo/cook.mp4");
        assert_eq!(
            args,
            [
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "x11grab",
                "-draw_mouse",
                "1",
                "-video_size",
                "1280x800",
                "-framerate",
                "8",
                "-i",
                ":1.0",
                "-an",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv420p",
                "-b:v",
                "700k",
                "-maxrate",
                "900k",
                "-bufsize",
                "1800k",
                "-movflags",
                "+frag_keyframe+empty_moov+default_base_moof",
                "/workspace/.l1/cooks/demo/cook.mp4",
            ]
        );
        assert!(!args
            .iter()
            .any(|a| a == "-t" || a == "-tune" || a.contains("faststart")));
    }

    #[test]
    fn x11grab_target_matches_vnc_display_screen0() {
        assert_eq!(display_target(":1"), ":1.0");
        assert_eq!(display_target(":1.0"), ":1.0");
    }

    #[test]
    fn remux_is_stream_copy_faststart() {
        let args = cook_remux_args(
            "/workspace/.l1/cooks/demo/cook.mp4",
            "/workspace/.l1/cooks/demo/cook.faststart.mp4",
        );
        assert_eq!(
            args,
            [
                "-nostdin",
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-i",
                "/workspace/.l1/cooks/demo/cook.mp4",
                "-an",
                "-c:v",
                "copy",
                "-movflags",
                "+faststart",
                "/workspace/.l1/cooks/demo/cook.faststart.mp4",
            ]
        );
        assert!(!args
            .iter()
            .any(|a| a.contains("frag_keyframe") || a.contains("libx264")));
    }
}
