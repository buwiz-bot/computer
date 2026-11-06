//! Fork/exec fallback when in-process XTEST / GetImage is unavailable.

use std::process::Stdio;
use std::time::{Duration, Instant};

use smallvec::SmallVec;
use tokio::process::Command;

use crate::{CuaConfig, CuaError};

/// Bound so a wedged xdotool cannot fall through to the 15s `--sync` poll.
const XDOTOOL_TIMEOUT: Duration = Duration::from_millis(2500);

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
    let status = Command::new("scrot")
        .env("DISPLAY", display)
        .arg("-o")
        .arg(&tmp)
        .status()
        .await
        .map_err(|err| CuaError::Tool(format!("scrot: {err}")))?;
    if !status.success() {
        return Err(CuaError::Tool("scrot failed".into()));
    }
    let png = tokio::fs::read(&tmp)
        .await
        .map_err(|err| CuaError::Tool(err.to_string()))?;
    let _ = tokio::fs::remove_file(&tmp).await;
    Ok(png)
}

async fn run_capture(cmd: &mut Command) -> Result<Vec<u8>, CuaError> {
    let out = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|err| CuaError::Tool(err.to_string()))?;
    if !out.status.success() || out.stdout.is_empty() {
        return Err(CuaError::Tool(String::from_utf8_lossy(&out.stderr).into()));
    }
    Ok(out.stdout)
}

pub(crate) async fn xdotool(config: &CuaConfig, args: &[&str]) -> Result<(), CuaError> {
    if args.iter().any(|a| *a == "--sync") {
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
