//! Taking a screen's end of Chromium's DevTools pipe from its launcher (`box-chrome-pipe`).
//!
//! The launcher listens at `cdp.sock` in the screen's run folder, which only the desktop and
//! `boxsvc` may enter, and hands its end once, to a `boxsvc` peer. Before taking anything, this
//! side checks that what listens is the screen's own desktop user, as for X and the accessibility
//! bus: anybody who could bind there could otherwise give `box-exec` a browser of their own.
//!
//! The connection the end came over is kept beside the tools, as the lease on that browser
//! ([`Leased`]): dropping the tools says "let go" on it first and waits for the launcher's answer,
//! so a browser that still runs is started again (`box_chrome::pipe` has the protocol); a close
//! for a shutdown says nothing, and those browsers stay closed.

use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use box_chrome::pipe::{recv_fd, socket_path, LEASE_ACK_WAIT, LEASE_LET_GO};
use box_common::peer::peer_uid;
use serde_json::Value;

use crate::seam::{self, GoalBounds, GoalReport, PolicyLists, TabInfo, ToolFailure, Tools};
use crate::{BrowserError, Opener};

/// How long the launcher has to hand the pipe over.
const HANDOVER: Duration = Duration::from_secs(2);

/// Opens a screen's tools over its real pipe.
pub struct PipeOpener;

impl Opener for PipeOpener {
    fn open(&self, display: &str) -> Result<Box<dyn Tools>, BrowserError> {
        let (pipe, lease) = take(display)?;
        open_leased(pipe, lease, seam::open)
    }
}

/// The tools `open` puts on `pipe`, leased. `open` (`seam::open`) drops the pipe when its first call finds it closed. A copy is held across it,
// so that, as for a drop later, the launcher hears the let-go while box-exec's end is still
// open, and decides: a browser already gone counts for nothing.
fn open_leased(
    pipe: OwnedFd,
    lease: UnixStream,
    open: impl FnOnce(OwnedFd) -> Result<Box<dyn Tools>, ToolFailure>,
) -> Result<Box<dyn Tools>, BrowserError> {
    let held = pipe
        .try_clone()
        .map_err(|err| BrowserError::Unavailable(format!("the pipe can't be held: {err}")))?;
    match open(pipe) {
        Ok(tools) => {
            drop(held);
            Ok(Box::new(Leased::new(lease, tools)))
        }
        Err(err) => {
            let_go(lease);
            drop(held);
            Err(BrowserError::Unavailable(err.to_string()))
        }
    }
}

/// A screen's tools, with the lease on its browser: the connection its launcher handed the pipe
/// over on. Dropped (a failure that looked like a closed pipe), they write
/// `box_chrome::pipe::LEASE_LET_GO` on the lease and wait up to `LEASE_ACK_WAIT` for the
/// launcher's answer before the tools, and with them the pipe, go: the launcher decides while
/// box-exec's end is still open, so a Chromium that still ran is started again, and one a person
/// had already closed isn't. box-exec can't tell those two apart itself; the launcher can.
/// [`Tools::close`], for a shutdown, sends nothing: the pipe and the lease just close, and that
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
        // the let-go and its answer first; the tools, and the pipe, after this returns
        if let Some(lease) = self.lease.take() {
            let_go(lease);
        }
    }
}

/// Says the let-go on `lease` and waits up to `LEASE_ACK_WAIT` for the launcher's answer (or its
/// end of file), then closes the lease. The caller still holds its end of the pipe, and closes it
/// after.
fn let_go(mut lease: UnixStream) {
    let _ = lease.set_write_timeout(Some(LEASE_ACK_WAIT));
    if lease.write_all(&[LEASE_LET_GO]).is_ok() {
        let _ = lease.set_read_timeout(Some(LEASE_ACK_WAIT));
        // the answer, an end of file (the launcher gone), or the wait out
        let _ = lease.read(&mut [0u8; 1]);
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
        // No let-go: a lease that only closes means nothing, so this browser stays closed however
        // long Chromium takes to quit.
        drop(self.lease.take());
        self.tools.close();
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

    /// What the launcher sees: a let-go, answered, while the pipe is still open when the tools are
    /// dropped; nothing at all, the lease just closing, after a close for a shutdown.
    #[test]
    fn a_drop_says_let_go_before_the_pipe_goes_and_a_close_says_nothing() {
        struct Pipe {
            end: Option<UnixStream>,
            open_at_drop: Arc<AtomicBool>,
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
                self.open_at_drop
                    .store(self.end.is_some(), Ordering::SeqCst);
            }
        }
        // a launcher that records what came, and when, and answers a let-go
        let launcher = |chromium: UnixStream, conn: UnixStream| {
            std::thread::spawn(move || {
                let mut heard = Vec::new();
                let mut byte = [0u8; 1];
                while matches!((&conn).read(&mut byte), Ok(1)) {
                    // the pipe is still box-exec's while it decides
                    chromium.set_nonblocking(true).unwrap();
                    let open = !matches!((&chromium).read(&mut [0u8; 1]), Ok(0));
                    heard.push((byte[0], open));
                    (&conn).write_all(&[box_chrome::pipe::LEASE_ACK]).unwrap();
                }
                heard
            })
        };
        let open_at_drop = Arc::new(AtomicBool::new(false));
        let leased = || {
            let (lease, conn) = UnixStream::pair().unwrap();
            let (end, chromium) = UnixStream::pair().unwrap();
            let tools = Pipe {
                end: Some(end),
                open_at_drop: Arc::clone(&open_at_drop),
            };
            (
                Leased::new(lease, Box::new(tools)),
                launcher(chromium, conn),
            )
        };
        let (dropped, heard) = leased();
        drop(dropped);
        assert!(open_at_drop.load(Ordering::SeqCst));
        assert_eq!(heard.join().unwrap(), [(LEASE_LET_GO, true)]);

        let (mut closing, heard) = leased();
        let started = std::time::Instant::now();
        closing.close();
        drop(closing);
        assert!(started.elapsed() < LEASE_ACK_WAIT);
        assert!(heard.join().unwrap().is_empty());
        assert!(!open_at_drop.load(Ordering::SeqCst));
    }

    /// A first call that finds the pipe closed drops it inside `seam::open`; the launcher still
    /// hears the let-go while box-exec's end is open, and decides.
    #[test]
    fn an_open_whose_first_call_fails_says_let_go_before_the_pipe_goes() {
        let (lease, conn) = UnixStream::pair().unwrap();
        let (end, chromium) = UnixStream::pair().unwrap();
        let launcher = std::thread::spawn(move || {
            let mut byte = [0u8; 1];
            (&conn).read_exact(&mut byte).unwrap();
            chromium.set_nonblocking(true).unwrap();
            let open = !matches!((&chromium).read(&mut [0u8; 1]), Ok(0));
            (&conn).write_all(&[box_chrome::pipe::LEASE_ACK]).unwrap();
            // and once answered, the pipe goes
            chromium.set_nonblocking(false).unwrap();
            let closed = matches!((&chromium).read(&mut [0u8; 1]), Ok(0));
            (byte[0], open, closed)
        });
        let failed = open_leased(end.into(), lease, |pipe| {
            drop(pipe);
            Err(ToolFailure::Closed("pipe closed".into()))
        });
        assert!(matches!(failed, Err(BrowserError::Unavailable(_))));
        assert_eq!(launcher.join().unwrap(), (LEASE_LET_GO, true, true));
    }

    /// With no launcher to answer, a drop waits no longer than the bound.
    #[test]
    fn a_let_go_nobody_answers_costs_at_most_its_wait() {
        struct Gone;
        impl Tools for Gone {
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
        }
        let (lease, silent) = UnixStream::pair().unwrap();
        let started = std::time::Instant::now();
        drop(Leased::new(lease, Box::new(Gone)));
        let took = started.elapsed();
        assert!(took >= LEASE_ACK_WAIT - Duration::from_millis(50) && took < LEASE_ACK_WAIT * 2);
        drop(silent);
        // the launcher already gone: no wait at all
        let (lease, gone) = UnixStream::pair().unwrap();
        drop(gone);
        let started = std::time::Instant::now();
        drop(Leased::new(lease, Box::new(Gone)));
        assert!(started.elapsed() < Duration::from_millis(200));
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
