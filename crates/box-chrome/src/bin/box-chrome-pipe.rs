//! `box-chrome-pipe <socket> -- <chromium> [args…]`: a screen's Chromium on a DevTools pipe that
//! only `box-exec` gets (see `box_chrome::pipe`).
//!
//! Runs as the screen's desktop user, from the entrypoint's restart loop or from `box-chromium`. It
//! makes a socket pair, starts Chromium with `--remote-debugging-pipe` on one end as fds 3 and 4,
//! and offers the other end once at `<socket>`, to a peer the kernel says is `boxsvc`. It lives as
//! long as Chromium, passes on the signals it gets, and exits as Chromium did, so the restart loop
//! and `chrome.pid` see what they always saw.
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
use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::time::Duration;

use box_chrome::pipe::{peer_uid, send_fd};

/// The only user the pipe is handed to.
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
    drop(browser_end);
    CHILD.store(child.id() as i32, Ordering::SeqCst);
    if STOPPING.load(Ordering::SeqCst) {
        // told to go while it started: it hears it now (twice is no harm)
        // SAFETY: kill with a negative pid signals Chromium's own group alone.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGTERM);
        }
    }

    let offered = socket.clone();
    std::thread::spawn(move || hand_over(listener, exec_end, service_uid, &offered));

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
    // Chromium is gone; nothing of its group outlives it.
    kill_group(pid);
    // The lock is still this process's, so the socket, if it's there, is too.
    let _ = std::fs::remove_file(&socket);
    match status {
        Ok(status) => match (status.code(), status.signal()) {
            (Some(code), _) => ExitCode::from(code.clamp(0, 255) as u8),
            (None, Some(signal)) => ExitCode::from((128 + signal).clamp(0, 255) as u8),
            _ => ExitCode::FAILURE,
        },
        Err(err) => {
            eprintln!("box-chrome-pipe: lost Chromium: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Gives `exec_end` to the first `boxsvc` peer, then closes this process's copy and the socket.
/// Anybody else is hung up on.
fn hand_over(listener: UnixListener, exec_end: OwnedFd, service_uid: u32, socket: &Path) {
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
            return;
        }
    }
}
