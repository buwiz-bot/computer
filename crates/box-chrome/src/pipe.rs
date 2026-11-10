//! Chromium's DevTools pipe, and how it gets from a screen's desktop to `box-exec` and nobody else.
//!
//! `box-chrome-pipe` starts a screen's Chromium as that screen's desktop user, with
//! `--remote-debugging-pipe` on one end of a socket pair as its fds 3 and 4. It keeps the other end
//! and offers it once, at `cdp.sock` in the screen's run folder (`/run/box/x/<N>/run`, the desktop's,
//! group `boxsvc`, mode 0750: a command's user can't enter it). It hands the end over with
//! `SCM_RIGHTS` only to a peer the kernel says is `boxsvc`, then closes its own copy and removes
//! the socket. From then on `box-exec` holds the only other end of the browser's protocol. Nothing
//! listens on TCP.
//!
//! `box-exec`, for its part, checks that what listens at `cdp.sock` is that screen's own desktop
//! user before it takes what is offered, as it checks the screen's X server and accessibility bus.
//! These are the helpers both sides use; who is on the other end of a socket is
//! `box_common::peer::peer_uid`.
//!
//! The connection the end came over is kept open by both sides for as long as `box-exec` holds the
//! end: it's how the launcher tells why Chromium went. Chromium quits, with status 0, once its pipe
//! closes, as it does when a person closes it. If `box-exec` lets go of a browser that still works
//! (its pipe failed, say), it closes that connection first, and the launcher, seeing it closed when
//! Chromium exits 0, exits [`PIPE_LET_GO`] instead, so the screen's restart loop starts a new
//! Chromium with a fresh pipe. A close `box-exec` means (its shutdown) drops the pipe first and
//! keeps the connection until the launcher is gone, so that browser stays closed.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

/// The launcher's status when Chromium quit because `box-exec` let go of its pipe while it still
/// worked: a status the restart loops (`docker/entrypoint.sh`, `docker/box-chromium`) start it
/// again on, where a 0, a person closing the browser, is left closed. `EX_TEMPFAIL`.
pub const PIPE_LET_GO: u8 = 75;

/// The socket's name in a screen's run folder.
pub const SOCKET_NAME: &str = "cdp.sock";

/// Where screen `slot`'s launcher offers its pipe.
pub fn socket_path(slot: u16) -> PathBuf {
    PathBuf::from(format!("/run/box/x/{slot}/run/{SOCKET_NAME}"))
}

/// Room for one fd's control message, aligned for `cmsghdr`.
fn control_buffer() -> (Vec<u64>, usize) {
    // SAFETY: CMSG_SPACE is arithmetic on its argument.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    (vec![0u64; space.div_ceil(8)], space)
}

/// Sends `fd` over `stream`, with one byte so the message is never empty.
pub fn send_fd(stream: &UnixStream, fd: BorrowedFd<'_>) -> io::Result<()> {
    let byte = *b"p";
    let mut iov = libc::iovec {
        iov_base: byte.as_ptr() as *mut libc::c_void,
        iov_len: 1,
    };
    let (mut control, space) = control_buffer();
    // SAFETY: msghdr is plain data, and zero is a valid value of each of its fields.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: `msg` points at a control buffer with room for one cmsghdr and one fd, so the first
    // header and its data are inside it.
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&msg);
        if header.is_null() {
            return Err(io::Error::other("no room for the control message"));
        }
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<RawFd>(), fd.as_raw_fd());
    }
    #[cfg(target_os = "linux")]
    let flags = libc::MSG_NOSIGNAL;
    #[cfg(not(target_os = "linux"))]
    let flags = 0;
    // SAFETY: every pointer in `msg` is valid for the call.
    let sent = unsafe { libc::sendmsg(stream.as_raw_fd(), &msg, flags) };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    if sent != 1 {
        return Err(io::Error::other("the fd went with no byte"));
    }
    Ok(())
}

/// Takes the one fd sent over `stream`. Anything else, no fd or more than one, is an error, and
/// every fd that came is closed.
pub fn recv_fd(stream: &UnixStream) -> io::Result<OwnedFd> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let (mut control, space) = control_buffer();
    // Room for a few more fds than expected, so a sender that sends several is seen doing it.
    control.resize(control.len() * 4, 0);
    // SAFETY: msghdr is plain data, and zero is a valid value of each of its fields.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = (space * 4) as _;
    #[cfg(target_os = "linux")]
    let flags = libc::MSG_CMSG_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = 0;
    // SAFETY: every pointer in `msg` is valid for the call.
    let got = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, flags) };
    if got < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut fds = Vec::new();
    // SAFETY: the kernel filled the control buffer and set its length; the CMSG macros walk only
    // inside it, and each SCM_RIGHTS payload is whole fds.
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&msg);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(header);
                let bytes = (*header).cmsg_len as usize - (data as usize - header as usize);
                for i in 0..bytes / std::mem::size_of::<RawFd>() {
                    let fd = std::ptr::read_unaligned(data.cast::<RawFd>().add(i));
                    fds.push(OwnedFd::from_raw_fd(fd));
                }
            }
            header = libc::CMSG_NXTHDR(&msg, header);
        }
    }
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::other("more was sent than one fd"));
    }
    match fds.len() {
        0 => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "nothing was offered: the pipe was already taken",
        )),
        1 => Ok(fds.remove(0)),
        n => Err(io::Error::other(format!("{n} fds were sent, not one"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::AsFd;

    #[test]
    fn an_fd_goes_across_and_still_works() {
        let (a, b) = UnixStream::pair().unwrap();
        let (mut keep, give) = UnixStream::pair().unwrap();
        send_fd(&a, give.as_fd()).unwrap();
        drop(give);
        let mut got = UnixStream::from(recv_fd(&b).unwrap());
        got.write_all(b"hi").unwrap();
        let mut buf = [0u8; 2];
        keep.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hi");
    }

    #[test]
    fn a_message_with_no_fd_is_an_error() {
        let (mut a, b) = UnixStream::pair().unwrap();
        a.write_all(b"x").unwrap();
        assert_eq!(recv_fd(&b).unwrap_err().kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn a_screen_s_socket_is_in_its_run_folder() {
        assert_eq!(socket_path(3), PathBuf::from("/run/box/x/3/run/cdp.sock"));
    }
}
