//! Taking a screen's end of Chromium's DevTools pipe from its launcher (`box-chrome-pipe`).
//!
//! The launcher listens at `cdp.sock` in the screen's run folder, which only the desktop and
//! `boxsvc` may enter, and hands its end once, to a `boxsvc` peer. Before taking anything, this
//! side checks that what listens is the screen's own desktop user, as for X and the accessibility
//! bus: anybody who could bind there could otherwise give `box-exec` a browser of their own.
//!
//! The connection the end came over is kept beside the tools, as the lease on that browser
//! ([`Leased`]): letting go of a browser that still works closes the lease first, so the launcher
//! knows to start Chromium again (`box_chrome::pipe::PIPE_LET_GO`); a close for a shutdown lets
//! the browser go first, and it stays closed.

use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use box_chrome::pipe::{recv_fd, socket_path};
use box_common::peer::peer_uid;
use serde_json::Value;

use crate::seam::{self, GoalBounds, GoalReport, PolicyLists, TabInfo, ToolFailure, Tools};
use crate::{BrowserError, Opener};

/// How long the launcher has to hand the pipe over.
const HANDOVER: Duration = Duration::from_secs(2);
/// How long a close for a shutdown waits for the launcher to be gone, so Chromium is seen to have
/// quit while the lease was still held.
const CLOSE_WAIT: Duration = Duration::from_secs(3);

/// Opens a screen's tools over its real pipe.
pub struct PipeOpener;

impl Opener for PipeOpener {
    fn open(&self, display: &str) -> Result<Box<dyn Tools>, BrowserError> {
        let (pipe, lease) = take(display)?;
        // A browser already gone takes its lease with it: there is nothing to start again.
        let tools = seam::open(pipe).map_err(|err| BrowserError::Unavailable(err.to_string()))?;
        Ok(Box::new(Leased::new(lease, tools)))
    }
}

/// A screen's tools, with the lease on its browser: the connection its launcher handed the pipe
/// over on. Dropped, the lease closes before the tools (and with them the pipe) do, so the
/// launcher sees box-exec let go of a working browser, and it is started again. [`Tools::close`],
/// for a shutdown, lets the pipe go first and holds the lease until the launcher is gone, so that
/// browser stays closed.
pub struct Leased {
    lease: Option<UnixStream>,
    tools: Box<dyn Tools>,
}

impl Leased {
    pub fn new(lease: UnixStream, tools: Box<dyn Tools>) -> Self {
        Self {
            lease: Some(lease),
            tools,
        }
    }
}

impl Drop for Leased {
    fn drop(&mut self) {
        // the lease first; the tools, and the pipe, after this returns
        drop(self.lease.take());
    }
}

impl Tools for Leased {
    fn call(&mut self, name: &str, args: &Value) -> Result<Value, ToolFailure> {
        self.tools.call(name, args)
    }

    fn set_policy(&mut self, policy: &PolicyLists) -> Result<(), ToolFailure> {
        self.tools.set_policy(policy)
    }

    fn run_goal(&mut self, goal: &str, bounds: &GoalBounds) -> Result<GoalReport, ToolFailure> {
        self.tools.run_goal(goal, bounds)
    }

    fn tabs(&mut self) -> Result<Vec<TabInfo>, ToolFailure> {
        self.tools.tabs()
    }

    fn open_tab(&mut self, url: &str) -> Result<String, ToolFailure> {
        self.tools.open_tab(url)
    }

    fn switch_tab(&mut self, id: &str) -> Result<(), ToolFailure> {
        self.tools.switch_tab(id)
    }

    fn close_tab(&mut self, id: &str) -> Result<(), ToolFailure> {
        self.tools.close_tab(id)
    }

    fn close(&mut self) {
        self.tools.close();
        // The launcher closes its side once Chromium is gone and it exits: until then, the lease
        // says this close was meant.
        if let Some(mut lease) = self.lease.take() {
            let _ = lease.set_read_timeout(Some(CLOSE_WAIT));
            let mut buf = [0u8; 64];
            while matches!(lease.read(&mut buf), Ok(n) if n > 0) {}
        }
    }
}

/// `display`'s pipe end, from its launcher, and the lease on it: the connection it came over.
pub fn take(display: &str) -> Result<(OwnedFd, UnixStream), BrowserError> {
    let slot = box_desktop::display_number(display)
        .ok_or_else(|| BrowserError::Invalid(format!("display must be :N, not {display:?}")))?;
    take_at(&socket_path(slot), expected_listener(slot)?)
        .map_err(|why| BrowserError::Unavailable(format!("{display} has no browser pipe: {why}")))
}

/// The user who must be listening for `slot`, or `None` outside the box (no
/// `BOX_RUN_AS=agents`), where there are no users to tell apart. In the box, a
/// `BOX_SHARED_DISPLAY` that isn't a display name refuses: no screen is taken for the main one.
fn expected_listener(slot: u16) -> Result<Option<u32>, BrowserError> {
    if std::env::var("BOX_RUN_AS").as_deref() != Ok("agents") {
        return Ok(None);
    }
    box_desktop::screen_owner(slot)
        .map(Some)
        .map_err(|why| BrowserError::Unavailable(why.into()))
}

/// The fd offered at `socket`, if what listens there is `expected` (any user when `None`), and the
/// connection it came over.
pub fn take_at(socket: &Path, expected: Option<u32>) -> Result<(OwnedFd, UnixStream), String> {
    let stream =
        UnixStream::connect(socket).map_err(|err| format!("{}: {err}", socket.display()))?;
    if let Some(expected) = expected {
        let peer = peer_uid(&stream).map_err(|err| err.to_string())?;
        if peer != expected {
            return Err("what listens there isn't this screen's desktop".into());
        }
    }
    stream
        .set_read_timeout(Some(HANDOVER))
        .map_err(|err| err.to_string())?;
    let pipe = recv_fd(&stream).map_err(|err| err.to_string())?;
    Ok((pipe, stream))
}

#[cfg(test)]
mod tests {
    use super::*;
    use box_chrome::pipe::send_fd;
    use std::io::{Read, Write};
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[test]
    fn the_offered_end_is_taken_from_its_own_user() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("cdp.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (mut browser, offered) = UnixStream::pair().unwrap();
        let launcher = std::thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            send_fd(&conn, offered.as_fd()).unwrap();
        });
        // SAFETY: geteuid has no preconditions.
        let me = unsafe { libc::geteuid() };
        let (pipe, _lease) = take_at(&socket, Some(me)).unwrap();
        let mut pipe = UnixStream::from(pipe);
        launcher.join().unwrap();
        pipe.write_all(b"{}\0").unwrap();
        let mut buf = [0u8; 3];
        browser.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"{}\0");
    }

    /// What the launcher sees: its lease closed before Chromium's pipe when box-exec lets go, and
    /// still held when a close for a shutdown has let the pipe go.
    #[test]
    fn a_lease_closes_before_the_pipe_and_after_a_close() {
        struct Pipe {
            end: Option<UnixStream>,
            launcher: Arc<UnixStream>,
            lease_closed_first: Arc<AtomicBool>,
        }
        impl Tools for Pipe {
            fn call(&mut self, _: &str, _: &Value) -> Result<Value, ToolFailure> {
                unreachable!()
            }
            fn set_policy(&mut self, _: &PolicyLists) -> Result<(), ToolFailure> {
                unreachable!()
            }
            fn run_goal(&mut self, _: &str, _: &GoalBounds) -> Result<GoalReport, ToolFailure> {
                unreachable!()
            }
            fn tabs(&mut self) -> Result<Vec<TabInfo>, ToolFailure> {
                unreachable!()
            }
            fn open_tab(&mut self, _: &str) -> Result<String, ToolFailure> {
                unreachable!()
            }
            fn switch_tab(&mut self, _: &str) -> Result<(), ToolFailure> {
                unreachable!()
            }
            fn close_tab(&mut self, _: &str) -> Result<(), ToolFailure> {
                unreachable!()
            }
            fn close(&mut self) {
                self.end = None;
            }
        }
        impl Drop for Pipe {
            fn drop(&mut self) {
                if self.end.is_some() {
                    self.launcher.set_nonblocking(true).unwrap();
                    let gone = matches!((&*self.launcher).read(&mut [0u8; 1]), Ok(0));
                    self.lease_closed_first.store(gone, Ordering::SeqCst);
                }
            }
        }
        let leased = |first: &Arc<AtomicBool>| {
            let (lease, launcher) = UnixStream::pair().unwrap();
            let (end, _chromium) = UnixStream::pair().unwrap();
            let launcher = Arc::new(launcher);
            let tools = Pipe {
                end: Some(end),
                launcher: Arc::clone(&launcher),
                lease_closed_first: Arc::clone(first),
            };
            (Leased::new(lease, Box::new(tools)), launcher)
        };
        let first = Arc::new(AtomicBool::new(false));
        let (dropped, _launcher) = leased(&first);
        drop(dropped);
        assert!(
            first.load(Ordering::SeqCst),
            "the pipe closed before the lease"
        );

        // a close: the pipe goes, and the lease is held until the launcher hangs up
        let (mut closing, launcher) = leased(&first);
        let hang_up = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            launcher.shutdown(std::net::Shutdown::Both).unwrap();
        });
        let started = std::time::Instant::now();
        closing.close();
        assert!(started.elapsed() >= Duration::from_millis(150));
        assert!(started.elapsed() < CLOSE_WAIT);
        hang_up.join().unwrap();
    }

    #[test]
    fn a_listener_that_is_not_the_desktop_gets_nothing_asked() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("cdp.sock");
        let _listener = UnixListener::bind(&socket).unwrap();
        // SAFETY: geteuid has no preconditions.
        let someone_else = unsafe { libc::geteuid() } + 1;
        let err = take_at(&socket, Some(someone_else)).unwrap_err();
        assert!(err.contains("isn't this screen's desktop"), "{err}");
    }

    #[test]
    fn a_launcher_that_sends_nothing_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("cdp.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let hold = std::thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            std::thread::sleep(HANDOVER + Duration::from_millis(500));
            drop(conn);
        });
        assert!(take_at(&socket, None).is_err());
        hold.join().unwrap();
    }

    #[test]
    fn a_screen_whose_launcher_offers_nothing_is_unavailable() {
        assert!(matches!(
            PipeOpener.open(":1"),
            Err(BrowserError::Unavailable(_))
        ));
    }
}
