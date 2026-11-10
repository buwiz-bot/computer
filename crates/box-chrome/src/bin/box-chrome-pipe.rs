//! `box-chrome-pipe <socket> -- <chromium> [args…]`: a screen's Chromium on a DevTools pipe that
//! only `box-exec` gets (see `box_chrome::pipe`).
//!
//! Runs as the screen's desktop user, from the entrypoint's restart loop or from `box-chromium`. It
//! makes a socket pair, starts Chromium with `--remote-debugging-pipe` on one end as fds 3 and 4,
//! and offers the other end once at `<socket>`, to a peer the kernel says is `boxsvc`. It lives as
//! long as Chromium, passes on the signals it gets, and exits as Chromium did, so the restart loop
//! and `chrome.pid` see what they always saw, with one exception: it keeps the connection it handed
//! the end over on (the lease), and when `box-exec` says there that it is letting go of the pipe
//! while Chromium still runs with it open, a 0 from Chromium after that is [`PIPE_LET_GO`], so the
//! loop starts Chromium again (`box_chrome::pipe` has the protocol).
//!
//! Chromium runs in a process group of its own. A signal to stop goes to that group; if Chromium
//! hasn't gone [`GRACE`] later, the group is killed. When Chromium exits, for whatever reason,
//! what's left of its group is killed too.
//!
//! It fails closed: if this screen's pipe is already offered by another launcher (Chromium then
//! hands the new window to the running instance), if the folder the socket would be in is open to
//! others, or if there is no `boxsvc` user, Chromium starts as it did before, with no pipe.

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use box_chrome::pipe::{send_fd, LEASE_ACK, LEASE_LET_GO, PIPE_LET_GO};
use box_common::peer::peer_uid;

/// The only user the pipe is handed to. `box-exec` and `box-host` both run as it, and the kernel
/// can't tell this launcher which program a peer is: another user's `/proc/<pid>/exe` is closed to
/// it, and a process's name is its own to set. Only a user of `box-exec`'s own could (docs/USERS.md).
const SERVICE_USER: &str = "boxsvc";

/// Chromium's pid, which is also its process group's id, for the signal handler.
static CHILD: AtomicI32 = AtomicI32::new(0);
/// Set once a signal to stop has been passed on.
static STOPPING: AtomicBool = AtomicBool::new(false);

/// How long Chromium has to go after it's told to, before its whole group is killed.
const GRACE: Duration = Duration::from_secs(3);
/// How often the launcher looks whether Chromium is gone.
const LOOK: Duration = Duration::from_millis(100);

extern "C" fn pass_on(signal: libc::c_int) {
    // Told to go before Chromium is up, too: then it isn't started, or is told as soon as it is.
    STOPPING.store(true, Ordering::SeqCst);
    let child = CHILD.load(Ordering::SeqCst);
    if child > 0 {
        // SAFETY: kill is async-signal-safe. The group is Chromium's own (setpgid below), so this
        // reaches its helpers too.
        unsafe {
            libc::kill(-child, signal);
        }
    }
}

/// Kills Chromium's group, whatever of it is left. A group that's gone already is no error.
fn kill_group(child: i32) {
    // SAFETY: kill with a negative pid signals that process group; nothing else is touched.
    unsafe {
        libc::kill(-child, libc::SIGKILL);
    }
}

/// What's needed to start Chromium on a pipe.
struct Offer {
    socket: PathBuf,
    /// Held for the life of this process: one offer per screen.
    _lock: File,
    listener: UnixListener,
    browser_end: OwnedFd,
    exec_end: OwnedFd,
    service_uid: u32,
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let Some(split) = args.iter().position(|a| a == "--") else {
        eprintln!("usage: box-chrome-pipe <socket> -- <chromium> [args…]");
        return ExitCode::from(2);
    };
    if split != 1 || args.len() < 3 {
        eprintln!("usage: box-chrome-pipe <socket> -- <chromium> [args…]");
        return ExitCode::from(2);
    }
    let socket = PathBuf::from(&args[0]);
    let program = &args[2];
    let rest = &args[3..];
    #[cfg(target_os = "linux")]
    // SAFETY: prctl with these arguments only changes this process's dumpable flag.
    unsafe {
        // It holds the other end until box-exec takes it: nothing else of this user's need read it.
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
    }

    let offer = match prepare(&socket) {
        Ok(offer) => offer,
        Err(why) => {
            eprintln!("box-chrome-pipe: no DevTools pipe on this start: {why}");
            let err = Command::new(program).args(rest).exec();
            eprintln!("box-chrome-pipe: cannot start {program:?}: {err}");
            return ExitCode::from(127);
        }
    };
    run(offer, program, rest)
}

/// The lock, the socket and the pair, or why there's no pipe this time.
fn prepare(socket: &Path) -> Result<Offer, String> {
    let service_uid = service_uid().ok_or_else(|| format!("there is no {SERVICE_USER} user"))?;
    let folder = socket
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or("the socket needs a folder")?;
    let meta =
        std::fs::symlink_metadata(folder).map_err(|e| format!("{}: {e}", folder.display()))?;
    // SAFETY: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    if !meta.is_dir() || meta.uid() != me || meta.mode() & 0o007 != 0 {
        return Err(format!(
            "{} must be a folder of this user's that others can't enter",
            folder.display()
        ));
    }
    let lock_path = socket.with_extension("lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&lock_path)
        .map_err(|e| format!("{}: {e}", lock_path.display()))?;
    // SAFETY: flock on an fd this process owns.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err("this screen's Chromium already has its pipe".into());
    }
    match std::fs::remove_file(socket) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", socket.display())),
    }
    let listener = UnixListener::bind(socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    // The folder keeps everyone else out; boxsvc isn't this socket's group, so the socket itself
    // is open, and the peer check decides.
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o666))
        .map_err(|e| format!("{}: {e}", socket.display()))?;
    let (browser_end, exec_end) = UnixStream::pair().map_err(|e| e.to_string())?;
    Ok(Offer {
        socket: socket.to_path_buf(),
        _lock: lock,
        listener,
        browser_end: browser_end.into(),
        exec_end: exec_end.into(),
        service_uid,
    })
}

fn service_uid() -> Option<u32> {
    let name = std::ffi::CString::new(SERVICE_USER).ok()?;
    // SAFETY: getpwnam is called once, before any other thread exists; the entry is read at once.
    let entry = unsafe { libc::getpwnam(name.as_ptr()) };
    if entry.is_null() {
        return None;
    }
    // SAFETY: a non-null entry from getpwnam points at a valid passwd.
    Some(unsafe { (*entry).pw_uid })
}

fn run(offer: Offer, program: &OsString, rest: &[OsString]) -> ExitCode {
    let Offer {
        socket,
        _lock,
        listener,
        browser_end,
        exec_end,
        service_uid,
    } = offer;
    // Before Chromium starts, so a signal never finds the launcher with the default action and
    // leaves Chromium behind.
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT] {
        // SAFETY: the handler only stores and loads atomics and calls kill.
        unsafe {
            libc::signal(signal, pass_on as *const () as libc::sighandler_t);
        }
    }
    let raw = browser_end.as_raw_fd();
    // SAFETY: getpid takes nothing and can't fail.
    let launcher = unsafe { libc::getpid() };
    let mut command = Command::new(program);
    command.arg("--remote-debugging-pipe").args(rest);
    // SAFETY: only async-signal-safe calls (prctl, getppid, fcntl, dup2, close, setpgid) run
    // between fork and exec.
    unsafe {
        command.pre_exec(move || {
            // The kernel kills Chromium when the launcher dies, even by SIGKILL or the OOM
            // killer, which no handler sees; one already gone by now is no parent to outlive.
            #[cfg(target_os = "linux")]
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != launcher {
                return Err(io::Error::other("the launcher is gone"));
            }
            // Copied up out of the way first: the end may itself be fd 3 or 4, and dup2 onto
            // itself would leave it close-on-exec.
            let high = libc::fcntl(raw, libc::F_DUPFD, 10);
            if high < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::dup2(high, 3) < 0 || libc::dup2(high, 4) < 0 {
                return Err(io::Error::last_os_error());
            }
            libc::close(high);
            // A process group of its own, so the launcher can end all of it, helpers included.
            if libc::setpgid(0, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    if STOPPING.load(Ordering::SeqCst) {
        let _ = std::fs::remove_file(&socket);
        return ExitCode::from(143);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            eprintln!("box-chrome-pipe: cannot start {program:?}: {err}");
            let _ = std::fs::remove_file(&socket);
            return ExitCode::from(127);
        }
    };
    // Kept, not dropped: whether Chromium has shut its end of the pipe is read on this copy, and
    // box-exec sees its end close only once this copy goes too, after Chromium is reaped.
    let lease = Arc::new(Lease::new(browser_end, child.id() as i32));
    CHILD.store(child.id() as i32, Ordering::SeqCst);
    if STOPPING.load(Ordering::SeqCst) {
        // told to go while it started: it hears it now (twice is no harm)
        // SAFETY: kill with a negative pid signals Chromium's own group alone.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGTERM);
        }
    }

    let offered = socket.clone();
    let held = Arc::clone(&lease);
    std::thread::spawn(move || hand_over(listener, exec_end, service_uid, &offered, &held));

    let pid = child.id() as i32;
    let mut told: Option<std::time::Instant> = None;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(err) => break Err(err),
        }
        if STOPPING.load(Ordering::SeqCst) {
            let since = *told.get_or_insert_with(std::time::Instant::now);
            if since.elapsed() >= GRACE {
                // Told to go and still here: it goes now.
                kill_group(pid);
            }
        }
        std::thread::sleep(LOOK);
    };
    // Chromium is gone; nothing of its group outlives it. A let-go box-exec sends from now on
    // comes after Chromium's exit, and counts for nothing.
    let let_go = lease.reaped();
    kill_group(pid);
    // The lock is still this process's, so the socket, if it's there, is too.
    let _ = std::fs::remove_file(&socket);
    match status {
        Ok(status) => {
            let stopping = STOPPING.load(Ordering::SeqCst);
            let code = exit_code(status.code(), status.signal(), let_go, stopping);
            if code == PIPE_LET_GO {
                eprintln!("box-chrome-pipe: box-exec let go of the pipe, and Chromium quit");
            }
            ExitCode::from(code)
        }
        Err(err) => {
            eprintln!("box-chrome-pipe: lost Chromium: {err}");
            ExitCode::FAILURE
        }
    }
}

/// The launcher's status for Chromium's: Chromium's own, or 128 + the signal that ended it; but a
/// 0 after box-exec let go of the pipe (`let_go`) is [`PIPE_LET_GO`], unless the launcher itself
/// was told to stop.
fn exit_code(code: Option<i32>, signal: Option<i32>, let_go: bool, stopping: bool) -> u8 {
    match (code, signal) {
        (Some(0), _) if let_go && !stopping => PIPE_LET_GO,
        (Some(code), _) => code.clamp(0, 255) as u8,
        (None, Some(signal)) => (128 + signal).clamp(0, 255) as u8,
        _ => 1,
    }
}

/// What the launcher knows of the pipe after the hand-over: its own copy of Chromium's end, the
/// child, and whether box-exec let go of the pipe while Chromium still ran with it open.
struct Lease {
    /// `None` once Chromium is reaped.
    watch: Mutex<Option<OwnedFd>>,
    child: i32,
    let_go: AtomicBool,
}

impl Lease {
    fn new(watch: OwnedFd, child: i32) -> Self {
        Self {
            watch: Mutex::new(Some(watch)),
            child,
            let_go: AtomicBool::new(false),
        }
    }

    /// box-exec says it is letting go: it counts only if Chromium is still alive and hasn't shut its
    /// end of the pipe, so its exit, when it comes, comes from that let-go. A browser a person
    /// closed has shut its pipe or exited before box-exec could hear of it, so a let-go box-exec
    /// sends in answer to that close counts for nothing. The decision is taken under the lock
    /// [`Self::reaped`] takes, so it never straddles the reap.
    fn let_go_now(&self) -> bool {
        let watch = self.watch.lock().unwrap_or_else(|p| p.into_inner());
        let why_not = match watch.as_ref() {
            None => Some("Chromium was reaped"),
            Some(_) if exited(self.child) => Some("Chromium had exited"),
            Some(fd) if hung_up(fd) => Some("Chromium had shut its pipe"),
            Some(_) => None,
        };
        match why_not {
            None => {
                self.let_go.store(true, Ordering::SeqCst);
                eprintln!("box-chrome-pipe: box-exec let go while Chromium ran: counted");
            }
            Some(why) => eprintln!("box-chrome-pipe: box-exec let go, not counted: {why}"),
        }
        why_not.is_none()
    }

    /// Chromium is reaped: the launcher's copy of its end goes, and the answer is whether box-exec
    /// had let go while it ran.
    fn reaped(&self) -> bool {
        let mut watch = self.watch.lock().unwrap_or_else(|p| p.into_inner());
        drop(watch.take());
        self.let_go.load(Ordering::SeqCst)
    }

    /// The lease, as box-exec speaks on it: a [`LEASE_LET_GO`] is decided and answered with
    /// [`LEASE_ACK`], whatever was decided, so box-exec closes its end of the pipe only after the
    /// decision. Anything else is ignored; the connection closing ends it, and means nothing.
    fn serve(&self, mut conn: &UnixStream) {
        let mut byte = [0u8; 1];
        loop {
            match conn.read(&mut byte) {
                Ok(0) => return,
                Ok(_) if byte[0] == LEASE_LET_GO => {
                    self.let_go_now();
                    if conn.write_all(&[LEASE_ACK]).is_err() {
                        return;
                    }
                }
                Ok(_) => {}
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
    }
}

/// Whether `child` has exited, reaped or not, without reaping it: the reap is the main loop's.
fn exited(child: i32) -> bool {
    // SAFETY: siginfo_t is plain data, and zero is a valid value of it.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: waitid with WNOWAIT only reads the child's state into `info`, which it owns.
    let got = unsafe {
        libc::waitid(
            libc::P_PID,
            child as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if got != 0 {
        // ECHILD: already reaped
        return true;
    }
    // SAFETY: waitid filled `info`; si_pid is 0 when the child is still running.
    unsafe { info.si_pid() != 0 }
}

/// Whether Chromium's end of the pipe is shut: Chromium shut it on its way out, or box-exec's end is
/// closed already. Data waiting to be read is not a hang-up.
///
/// The watch is a copy of the very end Chromium holds as fds 3 and 4, so Chromium merely closing
/// those fds, as an exit does, shows nothing here ([`exited`] covers that). What shows is
/// Chromium's `shutdown()` of the socket, which it does as it starts to quit: measured in the image
/// on a `Browser.close`, the copy reads `POLLIN | POLLHUP | POLLRDHUP` and box-exec's end reads its
/// end of file about 50 ms before Chromium exits, with this copy held (the image smoke's
/// `inflight` case checks that a let-go sent in that window isn't counted, and says why).
fn hung_up(fd: &OwnedFd) -> bool {
    #[cfg(target_os = "linux")]
    let events = libc::POLLIN | libc::POLLRDHUP;
    #[cfg(not(target_os = "linux"))]
    let events = libc::POLLIN;
    let mut poll = libc::pollfd {
        fd: fd.as_raw_fd(),
        events,
        revents: 0,
    };
    // SAFETY: one pollfd this function owns, no wait.
    if unsafe { libc::poll(&mut poll, 1, 0) } < 0 {
        return true;
    }
    #[cfg(target_os = "linux")]
    let hup = libc::POLLHUP | libc::POLLRDHUP | libc::POLLERR;
    #[cfg(not(target_os = "linux"))]
    let hup = libc::POLLHUP | libc::POLLERR;
    poll.revents & hup != 0
}

/// Gives `exec_end` to the first `boxsvc` peer, then closes this process's copy and the socket, and
/// serves the lease on the connection it went over until it closes. Anybody else is hung up on.
fn hand_over(
    listener: UnixListener,
    exec_end: OwnedFd,
    service_uid: u32,
    socket: &Path,
    lease: &Lease,
) {
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        match peer_uid(&conn) {
            Ok(uid) if uid == service_uid => {}
            _ => continue,
        }
        let _ = conn.set_write_timeout(Some(Duration::from_secs(2)));
        if send_fd(&conn, exec_end.as_fd()).is_ok() {
            drop(exec_end);
            drop(listener);
            let _ = std::fs::remove_file(socket);
            lease.serve(&conn);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Child;

    /// A child that runs until killed, as Chromium would.
    fn running() -> Child {
        Command::new("sleep").arg("30").spawn().unwrap()
    }

    fn wait_exited(pid: i32) {
        for _ in 0..200 {
            if exited(pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the child never exited");
    }

    #[test]
    fn a_let_go_counts_while_chromium_runs_with_its_pipe_open() {
        let mut chromium = running();
        let (browser, _exec) = UnixStream::pair().unwrap();
        let lease = Lease::new(browser.into(), chromium.id() as i32);
        assert!(lease.let_go_now());
        chromium.kill().unwrap();
        chromium.wait().unwrap();
        assert!(lease.reaped());
    }

    #[test]
    fn a_let_go_after_chromium_exited_counts_for_nothing() {
        let mut chromium = Command::new("true").spawn().unwrap();
        let (browser, _exec) = UnixStream::pair().unwrap();
        let lease = Lease::new(browser.into(), chromium.id() as i32);
        // exited, not yet reaped: what a person's close looks like in the launcher's next 100 ms
        wait_exited(chromium.id() as i32);
        assert!(!lease.let_go_now());
        chromium.wait().unwrap();
        assert!(!lease.reaped());
    }

    #[test]
    fn a_let_go_once_chromium_shut_its_pipe_counts_for_nothing() {
        let mut chromium = running();
        let (browser, _exec) = UnixStream::pair().unwrap();
        let watch = browser.try_clone().unwrap();
        let lease = Lease::new(watch.into(), chromium.id() as i32);
        // Chromium, still alive, shuts its end on its way out, as it does (see `hung_up`)
        browser.shutdown(std::net::Shutdown::Both).unwrap();
        assert!(!lease.let_go_now());
        chromium.kill().unwrap();
        chromium.wait().unwrap();
        assert!(!lease.reaped());
    }

    #[test]
    fn a_let_go_after_box_exec_closed_the_pipe_counts_for_nothing() {
        let mut chromium = running();
        let (browser, exec) = UnixStream::pair().unwrap();
        let lease = Lease::new(browser.into(), chromium.id() as i32);
        drop(exec);
        assert!(!lease.let_go_now());
        chromium.kill().unwrap();
        chromium.wait().unwrap();
    }

    #[test]
    fn a_let_go_after_the_reap_counts_for_nothing() {
        let mut chromium = running();
        let (browser, _exec) = UnixStream::pair().unwrap();
        let lease = Lease::new(browser.into(), chromium.id() as i32);
        chromium.kill().unwrap();
        chromium.wait().unwrap();
        assert!(!lease.reaped());
        assert!(!lease.let_go_now());
    }

    #[test]
    fn the_lease_answers_a_let_go_and_a_bare_close_means_nothing() {
        let mut chromium = running();
        let (browser, _exec) = UnixStream::pair().unwrap();
        let lease = Arc::new(Lease::new(browser.into(), chromium.id() as i32));
        let (conn, mut box_exec) = UnixStream::pair().unwrap();
        let served = Arc::clone(&lease);
        let server = std::thread::spawn(move || served.serve(&conn));
        // a stray byte is ignored; the let-go is answered
        box_exec.write_all(b"x").unwrap();
        box_exec.write_all(&[LEASE_LET_GO]).unwrap();
        let mut ack = [0u8; 1];
        box_exec.read_exact(&mut ack).unwrap();
        assert_eq!(ack, [LEASE_ACK]);
        drop(box_exec);
        server.join().unwrap();
        assert!(lease.let_go.load(Ordering::SeqCst));

        let quiet = Arc::new(Lease::new(
            UnixStream::pair().unwrap().0.into(),
            chromium.id() as i32,
        ));
        let (conn, box_exec) = UnixStream::pair().unwrap();
        let served = Arc::clone(&quiet);
        let server = std::thread::spawn(move || served.serve(&conn));
        drop(box_exec);
        server.join().unwrap();
        assert!(!quiet.reaped());
        chromium.kill().unwrap();
        chromium.wait().unwrap();
    }

    #[test]
    fn only_a_clean_exit_after_box_exec_let_go_restarts() {
        // box-exec let go, and Chromium quit as it does when its pipe closes
        assert_eq!(exit_code(Some(0), None, true, false), PIPE_LET_GO);
        // a person closed it, or box-exec closed it on purpose: stays closed
        assert_eq!(exit_code(Some(0), None, false, false), 0);
        // the launcher was told to stop: as Chromium exited
        assert_eq!(exit_code(Some(0), None, true, true), 0);
        // a crash is a crash either way
        assert_eq!(exit_code(Some(21), None, true, false), 21);
        assert_eq!(exit_code(None, Some(9), true, false), 137);
        assert_eq!(exit_code(None, None, false, false), 1);
    }
}
