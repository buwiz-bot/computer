//! Who is on the other end of a connected unix socket, as the kernel has it.
//!
//! What listens on a screen's sockets (its X server, its accessibility bus, its browser pipe) must
//! be that screen's desktop user, and what connects to a launcher must be `boxsvc`. Both sides ask
//! here.

use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

/// The user on the other end of a connected unix socket, as the kernel has it (`SO_PEERCRED`).
#[cfg(target_os = "linux")]
pub fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are live and valid for writes for the length of the call, and `len`
    // is the size of what `cred` holds.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred.uid)
}

/// The user on the other end of a connected unix socket, as the kernel has it (`getpeereid`).
#[cfg(not(target_os = "linux"))]
pub fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: both out-parameters are valid for writes for the length of the call.
    let rc = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(uid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_peer_is_this_user() {
        let (a, _b) = UnixStream::pair().unwrap();
        // SAFETY: geteuid has no preconditions.
        assert_eq!(peer_uid(&a).unwrap(), unsafe { libc::geteuid() });
    }
}
