//! Taking a screen's end of Chromium's DevTools pipe from its launcher (`box-chrome-pipe`).
//!
//! The launcher listens at `cdp.sock` in the screen's run folder, which only the desktop and
//! `boxsvc` may enter, and hands its end once, to a `boxsvc` peer. Before taking anything, this
//! side checks that what listens is the screen's own desktop user, as for X and the accessibility
//! bus: anybody who could bind there could otherwise give `box-exec` a browser of their own.

use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use box_chrome::pipe::{recv_fd, socket_path};
use box_common::peer::peer_uid;

use crate::seam::{self, Tools};
use crate::{BrowserError, Opener};

/// How long the launcher has to hand the pipe over.
const HANDOVER: Duration = Duration::from_secs(2);

/// Opens a screen's tools over its real pipe.
pub struct PipeOpener;

impl Opener for PipeOpener {
    fn open(&self, display: &str) -> Result<Box<dyn Tools>, BrowserError> {
        let pipe = take(display)?;
        seam::open(pipe).map_err(|err| BrowserError::Unavailable(err.to_string()))
    }
}

/// `display`'s pipe end, from its launcher.
pub fn take(display: &str) -> Result<OwnedFd, BrowserError> {
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

/// The fd offered at `socket`, if what listens there is `expected` (any user when `None`).
pub fn take_at(socket: &Path, expected: Option<u32>) -> Result<OwnedFd, String> {
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
    recv_fd(&stream).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use box_chrome::pipe::send_fd;
    use std::io::{Read, Write};
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixListener;

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
        let mut pipe = UnixStream::from(take_at(&socket, Some(me)).unwrap());
        launcher.join().unwrap();
        pipe.write_all(b"{}\0").unwrap();
        let mut buf = [0u8; 3];
        browser.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"{}\0");
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
