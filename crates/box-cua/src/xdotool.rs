//! Fork/exec fallback when in-process XTEST / GetImage is unavailable.

use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

use smallvec::SmallVec;
use tokio::process::Command;

use crate::{CuaConfig, CuaError};

/// Bound so a wedged xdotool cannot fall through to the 15s `--sync` poll.
const XDOTOOL_TIMEOUT: Duration = Duration::from_millis(2500);
/// A whole-screen `import` or `scrot` takes well under a second; a wedged X server takes forever.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);
/// `wmctrl`, `xwininfo` and the window `xdotool` calls answer at once or not at all.
pub(crate) const WINDOW_TOOL_TIMEOUT: Duration = Duration::from_millis(2500);

/// Run an X tool to completion with its output captured, and kill it once `timeout` has passed.
/// A timeout, or a tool that cannot be started, is an error. The exit status is the caller's to
/// read: a non-zero one is a failure, never data.
pub(crate) async fn run_tool(cmd: &mut Command, timeout: Duration) -> Result<Output, CuaError> {
    let program = cmd.as_std().get_program().to_string_lossy().into_owned();
    // No cap on what it prints: a capture's PNG is all of it.
    match box_common::process::run_bounded(cmd, timeout, u64::MAX).await {
        Ok(out) => Ok(out),
        Err(err) if err.kind() != std::io::ErrorKind::TimedOut => {
            Err(CuaError::Tool(format!("{program}: {err}")))
        }
        Err(_) => {
            tracing::warn!(program, ms = timeout.as_millis() as u64, "X tool timed out");
            Err(CuaError::Tool(format!(
                "{program} timed out after {}ms",
                timeout.as_millis()
            )))
        }
    }
}

pub(crate) async fn capture_png(display: &str) -> Result<Vec<u8>, CuaError> {
    if let Ok(png) = run_capture(
        Command::new("import")
            .arg("-display")
            .arg(display)
            .arg("-window")
            .arg("root")
            .arg("png:-"),
    )
    .await
    {
        return Ok(png);
    }
    // TMPDIR is this service's own: in /tmp a command could leave a link at this name
    let tmp = std::env::temp_dir().join(format!("box-cua-{}.png", std::process::id()));
    let out = run_tool(
        Command::new("scrot")
            .env("DISPLAY", display)
            .arg("-o")
            .arg(&tmp),
        CAPTURE_TIMEOUT,
    )
    .await;
    if !matches!(&out, Ok(out) if out.status.success()) {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(match out {
            Err(err) => err,
            Ok(_) => CuaError::Tool("scrot failed".into()),
        });
    }
    let png = tokio::fs::read(&tmp)
        .await
        .map_err(|err| CuaError::Tool(err.to_string()))?;
    let _ = tokio::fs::remove_file(&tmp).await;
    Ok(png)
}

async fn run_capture(cmd: &mut Command) -> Result<Vec<u8>, CuaError> {
    let out = run_tool(cmd, CAPTURE_TIMEOUT).await?;
    if !out.status.success() || out.stdout.is_empty() {
        return Err(CuaError::Tool(String::from_utf8_lossy(&out.stderr).into()));
    }
    Ok(out.stdout)
}

pub(crate) async fn xdotool(config: &CuaConfig, args: &[&str]) -> Result<(), CuaError> {
    if args.contains(&"--sync") {
        return Err(CuaError::Tool(
            "xdotool --sync is not allowed (Xvfb can block ~15s)".into(),
        ));
    }
    let started = Instant::now();
    let child = Command::new("xdotool")
        .env("DISPLAY", &config.display)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| CuaError::Tool(format!("xdotool: {err}")))?;
    match tokio::time::timeout(XDOTOOL_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(out)) => {
            let ms = started.elapsed().as_millis() as u64;
            if !out.status.success() {
                let err = String::from_utf8_lossy(&out.stderr);
                tracing::warn!(ms, args = args.join(" "), error = %err, "xdotool failed");
                return Err(CuaError::Tool(format!("xdotool failed: {err}")));
            }
            if ms >= 50 {
                tracing::warn!(ms, args = args.join(" "), "xdotool slow");
            } else {
                tracing::debug!(ms, args = args.join(" "), "xdotool");
            }
            Ok(())
        }
        Ok(Err(err)) => Err(CuaError::Tool(format!("xdotool: {err}"))),
        Err(_) => {
            tracing::error!(
                ms = XDOTOOL_TIMEOUT.as_millis() as u64,
                args = args.join(" "),
                "xdotool timed out"
            );
            Err(CuaError::Tool(
                "xdotool timed out (2.5s); check XTEST on Xvfb".into(),
            ))
        }
    }
}

pub(crate) async fn xdotool_owned(config: &CuaConfig, args: &[String]) -> Result<(), CuaError> {
    let refs: SmallVec<[&str; 16]> = args.iter().map(String::as_str).collect();
    xdotool(config, &refs).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_tool_that_hangs_is_killed_at_its_timeout() {
        let started = Instant::now();
        let err = run_tool(Command::new("sleep").arg("10"), Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn a_tool_that_cannot_start_is_an_error() {
        let err = run_tool(
            &mut Command::new("/nonexistent/wmctrl"),
            WINDOW_TOOL_TIMEOUT,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("/nonexistent/wmctrl"), "{err}");
    }

    #[tokio::test]
    async fn a_failed_capture_is_an_error_not_a_picture() {
        let err = run_capture(Command::new("sh").args(["-c", "printf junk; exit 1"]))
            .await
            .unwrap_err();
        assert!(matches!(err, CuaError::Tool(_)));
    }
}
