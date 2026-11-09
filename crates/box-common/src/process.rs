//! One bounded way to run a helper tool (`xdotool`, `wmctrl`, `xdpyinfo`, `import`) from an async
//! handler: it never blocks a worker, and a tool that hangs is killed.

use std::process::{Output, Stdio};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

/// Run `command` to completion with stdin closed and stdout and stderr captured, at most
/// `max_output` bytes of each, and kill it once `timeout` has passed or when the returned future
/// is dropped. A timeout is an [`std::io::ErrorKind::TimedOut`] error, and a tool that cannot be
/// started is its spawn's error. The exit status is the caller's to read.
pub async fn run_bounded(
    command: &mut Command,
    timeout: Duration,
    max_output: u64,
) -> std::io::Result<Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    // Read while it runs, so a tool that fills a pipe still gets to exit. Past `max_output` the
    // pipe is closed, and the tool's next write fails.
    let run = async {
        let (status, stdout, stderr) = tokio::join!(
            child.wait(),
            read_capped(stdout, max_output),
            read_capped(stderr, max_output)
        );
        Ok(Output {
            status: status?,
            stdout,
            stderr,
        })
    };
    match tokio::time::timeout(timeout, run).await {
        Ok(out) => out,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("did not finish within {}ms", timeout.as_millis()),
        )),
    }
}

async fn read_capped(pipe: Option<impl AsyncRead + Unpin>, max: u64) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(pipe) = pipe {
        let _ = pipe.take(max).read_to_end(&mut buf).await;
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test]
    async fn a_tool_that_hangs_is_killed_at_its_timeout() {
        let started = Instant::now();
        let err = run_bounded(
            Command::new("sleep").arg("10"),
            Duration::from_millis(100),
            1024,
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn a_tool_that_finishes_gives_its_output_and_status() {
        let out = run_bounded(
            Command::new("sh").args(["-c", "echo hi; echo no >&2; exit 3"]),
            Duration::from_secs(5),
            1024,
        )
        .await
        .unwrap();
        assert_eq!(out.stdout, b"hi\n");
        assert_eq!(out.stderr, b"no\n");
        assert_eq!(out.status.code(), Some(3));
    }

    #[tokio::test]
    async fn output_past_the_cap_is_cut_and_the_tool_still_ends() {
        let out = run_bounded(
            Command::new("sh").args(["-c", "yes | head -c 100000"]),
            Duration::from_secs(5),
            10,
        )
        .await
        .unwrap();
        assert_eq!(out.stdout.len(), 10);
    }

    #[tokio::test]
    async fn a_tool_that_cannot_start_is_an_error() {
        let err = run_bounded(
            &mut Command::new("/nonexistent/tool"),
            Duration::from_secs(1),
            1024,
        )
        .await
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }
}
