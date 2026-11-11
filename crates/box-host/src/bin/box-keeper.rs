//! `box-keeper`: the one process in the box that reads the secrets, the parent of the rest, and the
//! only one that stays root.
//!
//! It sets the box up (`box-prepare`), then starts everything as the user it should be, and nothing
//! else in the box can act on it or them:
//!
//! - `box-host` and `box-exec` as `boxsvc`.
//! - the main screen's desktop as `box`, and each Bot's as that Bot's desktop user (`box-screen`).
//!   Each screen's voxl runs as its desktop's user too: X only shares memory with a client of its
//!   own user, and that's how voxl reads the screen. It holds the viewer password from an
//!   execute-only binary, as the daemons hold theirs, so a process of its user can't read it.
//! - commands as a user that has neither: `box-exec` drops to it for each one.
//!
//! This process reads the secrets and is not dumpable. `entrypoint.sh` never sees one: a shell
//! cannot be made safe that way. `box-host` and `box-exec` get what each needs in a staged file
//! they unlink, and voxl, which takes its password from argv or its environment only, in its
//! environment. Daemons start on what this process can see for itself, the display accepting
//! clients and the egress bearer unlinked, and not on anyone's word.
//!
//! `box-exec` asks for screens over a socket only its user can open, because a screen takes root:
//! a request is checked at both ends and runs `box-screen` with these words as arguments.

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io::{BufReader, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use box_common::{
    ensure_not_placeholder, ensure_viewer_password_strength, env_bool, env_nonempty,
    make_undumpable, read_secret_from_env, ScreenCall, KEEPER_SOCKET,
};
use box_desktop::{
    bots_dir, desktop_uid, display_number, slots_dir, DesktopConfig, VIEWER_PORT_BASE,
};
use box_host::keeper::{may_have_viewer, next_line, restart_delay, Request, STABLE};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;

const ENTRYPOINT: &str = "/usr/local/bin/entrypoint.sh";
const PREPARE: &str = "/usr/local/bin/box-prepare";
const BOX_SCREEN: &str = "/usr/local/bin/box-screen";
const SETPRIV: &str = "/usr/bin/setpriv";
const DEFAULT_CTL: &str = "/run/box/viewer.ctl";
const SVC_UID: u32 = 990;
const SVC_HOME: &str = "/run/box/svc";
const SVC_AUTH: &str = "/run/box/x/svc.auth";
const DESKTOP_UID: u32 = 1000;
const DESKTOP_HOME: &str = "/home/box/desktops/main";
/// `boxx` for its X socket, and `agent` for the folder its downloads land in.
const DESKTOP_GROUPS: &str = "--groups=1400,1500";
/// Screen requests at once: more than a computer has Bots, and each waits for one lock.
const SCREEN_REQUESTS: usize = 8;
/// The capabilities each keeps past its exec, which is why they're ambient. `box-exec` takes them
/// out of the ambient set as it starts, and holds them for itself.
const VOXL_CAPS: &str = "net_bind_service";
const HOST_CAPS: &str = "net_bind_service";
const EXEC_CAPS: &str = "net_bind_service,setuid,setgid,kill";
/// `box-screen` waits on its lock, then for an X server and a viewer.
const SCREEN_PATIENCE: Duration = Duration::from_secs(40);
const REQUEST_PATIENCE: Duration = Duration::from_secs(5);
/// Longer than any request. See `ScreenCall`.
const MAX_REQUEST: u64 = 16 * 1024;
const TICK: Duration = Duration::from_millis(200);
const STOP_PATIENCE: Duration = Duration::from_secs(8);
/// How often to look for the desktop before the daemons are up: this is on the wake path.
const START_POLL: Duration = Duration::from_millis(10);
/// How long a daemon gets to read and unlink what was staged for it. A busy machine is the only
/// reason to need more than milliseconds, and running out stops the box.
const UNLINK_PATIENCE: Duration = Duration::from_secs(10);

/// What this box runs. The keeper decides it once and tells `entrypoint.sh`, so the two cannot
/// read a flag differently.
#[derive(Clone, Copy)]
struct Features {
    desktop: bool,
    /// The tunnel (`BOX_EGRESS_TUNNEL`).
    egress: bool,
    /// The browser's proxy, under the computer's rules: `BOX_EGRESS_PROXY`, or the tunnel.
    proxy: bool,
}

impl Features {
    fn from_env() -> Self {
        let egress = env_bool("BOX_EGRESS_TUNNEL", false);
        Self {
            desktop: env_bool("BOX_DESKTOP", true),
            egress,
            proxy: egress || env_bool("BOX_EGRESS_PROXY", false),
        }
    }
}

/// The rules the box starts with, where the proxy reads them and box-host replaces them
/// (`box-prepare` made the folder, box-host's user's alone). From `BOX_EGRESS_RULES_FILE`, which
/// whoever started the box staged. None that reads, or none at all, is no file, and the proxy
/// refuses every site until rules are set, whichever the exit: never a box that browses with no
/// rules.
fn stage_rules() -> Result<(), String> {
    let path = PathBuf::from(
        env_nonempty("BOX_EGRESS_RULES_PATH")
            .unwrap_or_else(|| box_egress_tunnel::config::DEFAULT_RULES_PATH.into()),
    );
    let staged = env_nonempty("BOX_EGRESS_RULES_FILE").map(|file| {
        fs::File::open(&file)
            .and_then(|f| {
                use std::io::Read;
                let mut bytes = Vec::new();
                f.take(box_egress::rules::MAX_RULES_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)
                    .map(|_| bytes)
            })
            .map_err(|err| format!("it could not be read ({})", err.kind()))
    });
    let Some(rules) = starting_rules(staged) else {
        return Ok(());
    };
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&path)
        .map_err(|err| format!("egress rules: {err}"))?;
    file.write_all(rules.to_json().as_bytes())
        .map_err(|err| format!("egress rules: {err}"))?;
    std::os::unix::fs::chown(&path, Some(SVC_UID), Some(SVC_UID))
        .map_err(|err| format!("egress rules: {err}"))
}

/// The rules a box starts with, from what was staged: `None` (no site) when nothing was, or what
/// was doesn't read, and said so.
fn starting_rules(staged: Option<Result<Vec<u8>, String>>) -> Option<box_egress::Rules> {
    let read = match staged {
        None => {
            eprintln!(
                "the browser's proxy is on and BOX_EGRESS_RULES_FILE is not set: the browser refuses every site until rules are set (PUT /v1/egress/rules)."
            );
            return None;
        }
        Some(read) => read,
    };
    match read.and_then(|bytes| box_egress::Rules::parse(&bytes).map_err(|err| err.to_string())) {
        Ok(rules) => Some(rules),
        Err(why) => {
            eprintln!(
                "the egress rules in BOX_EGRESS_RULES_FILE were not used: {why}. The browser refuses every site until rules are set."
            );
            None
        }
    }
}

fn flag(on: bool) -> &'static str {
    if on {
        "1"
    } else {
        "0"
    }
}

struct Secrets {
    token: String,
    host_token: String,
    viewer_password: Option<String>,
    egress_bearer: Option<String>,
}

/// Read one secret. What's staged on a read-only mount stays until the Bot removes it, and that is
/// worth saying out loud at every start.
fn secret(name: &str) -> Result<Option<String>, String> {
    let path = env_nonempty(&format!("{name}_FILE"));
    if path.is_none() && env_nonempty(name).is_some() {
        eprintln!("{name} came as a value: it stays readable in /proc/1/environ. Use {name}_FILE.");
    }
    let value = read_secret_from_env(name, true).map_err(|err| err.to_string())?;
    if let Some(path) = path.filter(|path| Path::new(path).exists()) {
        eprintln!("{path} could not be unlinked: it stays until whoever staged it removes it.");
    }
    Ok(value)
}

impl Secrets {
    fn load(features: Features) -> Result<Self, String> {
        let token = secret("BOX_TOKEN")?.ok_or("BOX_TOKEN or BOX_TOKEN_FILE must be set")?;
        let host_token = secret("BOX_HOST_TOKEN")?.unwrap_or_else(|| token.clone());
        // The sample from .env.example has no exemption: refuse it before anything starts.
        ensure_not_placeholder("BOX_TOKEN", &token).map_err(|err| err.to_string())?;
        ensure_not_placeholder("BOX_HOST_TOKEN", &host_token).map_err(|err| err.to_string())?;
        let viewer_password = secret("BOX_VIEWER_PASSWORD")?;
        if features.desktop && viewer_password.is_none() {
            return Err(
                "BOX_VIEWER_PASSWORD or BOX_VIEWER_PASSWORD_FILE must be set when BOX_DESKTOP=1 \
                 (independent of BOX_TOKEN)"
                    .into(),
            );
        }
        // Here, before anything starts: voxl must never come up on a password box-exec refuses.
        if let Some(password) = &viewer_password {
            ensure_viewer_password_strength(password).map_err(|err| err.to_string())?;
        }
        let egress_bearer = secret("BOX_EGRESS_TUNNEL_BEARER")?;
        if let Some(bearer) = &egress_bearer {
            ensure_not_placeholder("BOX_EGRESS_TUNNEL_BEARER", bearer)
                .map_err(|err| err.to_string())?;
        }
        if features.egress && egress_bearer.is_none() {
            return Err("BOX_EGRESS_TUNNEL=1 requires BOX_EGRESS_TUNNEL_BEARER or \
                 BOX_EGRESS_TUNNEL_BEARER_FILE"
                .into());
        }
        // Children inherit neither the values nor where they came from.
        box_common::wipe_secret_environ();
        Ok(Self {
            token,
            host_token,
            viewer_password,
            egress_bearer,
        })
    }
}

/// Files a daemon reads and unlinks: no secret goes into a daemon's environment or argv.
struct Staging {
    dir: PathBuf,
    owner: u32,
}

impl Staging {
    /// A folder for `owner`, under `/run/box`, which `box-prepare` has just emptied.
    fn new(name: &str, owner: u32) -> Result<Self, String> {
        let dir = PathBuf::from(format!("/run/box/{name}"));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|err| format!("secret staging: {err}"))?;
        std::os::unix::fs::chown(&dir, Some(owner), Some(owner))
            .map_err(|err| format!("secret staging: {err}"))?;
        Ok(Self { dir, owner })
    }

    fn stage(&self, name: &str, value: &str) -> Result<PathBuf, String> {
        let path = self.dir.join(name);
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o400)
            .open(&path)
            .map_err(|err| format!("secret staging: {err}"))?;
        file.write_all(value.as_bytes())
            .map_err(|err| format!("secret staging: {err}"))?;
        std::os::unix::fs::chown(&path, Some(self.owner), Some(self.owner))
            .map_err(|err| format!("secret staging: {err}"))?;
        Ok(path)
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// `program` as `uid`, with `caps` and no groups.
fn as_user(uid: u32, program: &str, caps: &str) -> Command {
    let mut command = Command::new(SETPRIV);
    command
        .args([
            format!("--reuid={uid}"),
            format!("--regid={uid}"),
            "--clear-groups".to_string(),
            format!("--inh-caps={}", plus(caps)),
            format!("--ambient-caps={}", plus(caps)),
            program.to_string(),
        ])
        .current_dir("/");
    command
}

/// `program` as `boxsvc`. Its environment says where it lives, and X lets it into every screen.
fn service(program: &str, caps: &str) -> Command {
    let mut command = as_user(SVC_UID, program, caps);
    command
        .env("HOME", SVC_HOME)
        .env("TMPDIR", SVC_HOME)
        .env("XAUTHORITY", SVC_AUTH);
    command
}

/// `a,b` as `setpriv` wants it: `+a,+b`.
fn plus(caps: &str) -> String {
    caps.split(',')
        .map(|cap| format!("+{cap}"))
        .collect::<Vec<_>>()
        .join(",")
}

struct Viewer {
    child: Option<Child>,
    started: Instant,
    failures: u32,
    retry_at: Instant,
}

struct Viewers {
    /// `None` on a box with no desktop, which has no viewer. voxl makes up its own password when
    /// it is given none and logs it, so it never starts without this.
    password: Option<String>,
    main: u16,
    slots: PathBuf,
    running: HashMap<u16, Viewer>,
    /// Told to stop, and not yet reaped.
    leaving: Vec<Child>,
}

impl Viewers {
    fn spawn(&self, slot: u16, password: &str) -> std::io::Result<Child> {
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(format!("/run/box/log/voxl-{slot}.log"))?;
        let uid = desktop_uid(slot, self.main, &self.slots, &bots_dir())
            .ok_or_else(|| std::io::Error::other("this screen has no user"))?;
        as_user(uid, "voxl", VOXL_CAPS)
            .env("HOME", format!("/run/box/x/{slot}/run"))
            .env("XAUTHORITY", format!("/run/box/x/{slot}/auth"))
            .env("DISPLAY", format!(":{slot}"))
            .env("VOXL_PASSWORD", password)
            .args([
                "--addr=0.0.0.0".to_string(),
                format!("--port={}", VIEWER_PORT_BASE + slot),
                "--title=Buwiz computer".to_string(),
                "--audio-enabled=false".to_string(),
                "--microphone-enabled=false".to_string(),
                "--enable-resize=false".to_string(),
                "--file-transfers=none".to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .spawn()
    }

    fn up(&mut self, slot: u16) {
        if self.password.is_none()
            || self.running.contains_key(&slot)
            || !may_have_viewer(slot, self.main, &self.slots)
        {
            return;
        }
        let now = Instant::now();
        self.running.insert(
            slot,
            Viewer {
                child: None,
                started: now,
                failures: 0,
                retry_at: now,
            },
        );
        self.tick();
    }

    fn down(&mut self, slot: u16) {
        if let Some(Viewer {
            child: Some(child), ..
        }) = self.running.remove(&slot)
        {
            terminate(&child);
            self.leaving.push(child);
        }
    }

    /// Reap what stopped, and start what is due. Back off, so a voxl that cannot stay up does
    /// not spin the box.
    fn tick(&mut self) {
        self.leaving
            .retain_mut(|child| !matches!(child.try_wait(), Ok(Some(_))));
        let now = Instant::now();
        let slots: Vec<u16> = self.running.keys().copied().collect();
        for slot in slots {
            let exited = {
                let viewer = &mut self.running.get_mut(&slot).expect("a key of the map");
                viewer
                    .child
                    .as_mut()
                    .is_some_and(|child| !matches!(child.try_wait(), Ok(None)))
            };
            if exited {
                self.failed(slot, now, "exited");
            }
            let due = {
                let viewer = &self.running[&slot];
                viewer.child.is_none() && now >= viewer.retry_at
            };
            let password = self.password.clone().filter(|_| due);
            if let Some(password) = password {
                match self.spawn(slot, &password) {
                    Ok(child) => {
                        let viewer = self.running.get_mut(&slot).expect("a key of the map");
                        viewer.child = Some(child);
                        viewer.started = now;
                    }
                    Err(err) => self.failed(slot, now, &err.to_string()),
                }
            }
        }
    }

    fn failed(&mut self, slot: u16, now: Instant, why: &str) {
        let viewer = self.running.get_mut(&slot).expect("a key of the map");
        viewer.failures = if viewer.child.is_some() && viewer.started.elapsed() >= STABLE {
            0
        } else {
            viewer.failures + 1
        };
        viewer.child = None;
        let delay = restart_delay(viewer.failures.saturating_sub(1));
        viewer.retry_at = now + delay;
        eprintln!(
            "voxl on :{slot}: {why}; trying again in {}s",
            delay.as_secs()
        );
    }
}

fn terminate(child: &Child) {
    // SAFETY: kill(2) on a child this process has not reaped yet.
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
}

fn make_fifo(path: &Path) -> Result<(), String> {
    let _ = fs::remove_file(path);
    let raw = CString::new(path.as_os_str().as_bytes()).map_err(|err| err.to_string())?;
    // SAFETY: `raw` is a valid C string for the length of the call.
    if unsafe { libc::mkfifo(raw.as_ptr(), 0o600) } != 0 {
        return Err(format!(
            "mkfifo {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// The socket `box-exec` asks for screens on. Nobody else's user can open it.
fn screen_socket() -> Result<UnixListener, String> {
    let listener =
        UnixListener::bind(KEEPER_SOCKET).map_err(|err| format!("keeper socket: {err}"))?;
    std::os::unix::fs::chown(KEEPER_SOCKET, Some(SVC_UID), Some(SVC_UID))
        .and_then(|()| fs::set_permissions(KEEPER_SOCKET, PermissionsExt::from_mode(0o600)))
        .map_err(|err| format!("keeper socket: {err}"))?;
    Ok(listener)
}

/// One request: its line in, what `box-screen` printed out. `box-screen` gets a bare environment
/// and what it prints goes straight back: it's JSON, with an `error` when it failed.
async fn serve_screen_request(stream: tokio::net::UnixStream) {
    let (reader, mut writer) = stream.into_split();
    let mut line = String::new();
    let mut limited = tokio::io::BufReader::new(reader.take(MAX_REQUEST));
    let read = tokio::time::timeout(REQUEST_PATIENCE, limited.read_line(&mut line)).await;
    let answer = match (read, ScreenCall::parse(&line)) {
        (Ok(Ok(_)), Some(call)) => run_box_screen(&call).await,
        _ => br#"{"error":"that isn't a screen request"}"#.to_vec(),
    };
    let _ = writer.write_all(&answer).await;
}

/// More are waiting than the box should run. Told so, and at no cost: nothing is started for it.
async fn refuse_screen_request(mut stream: tokio::net::UnixStream) {
    let _ = stream
        .write_all(br#"{"error":"too many screen requests at once"}"#)
        .await;
}

async fn run_box_screen(call: &ScreenCall) -> Vec<u8> {
    let mut command = tokio::process::Command::new(BOX_SCREEN);
    command
        .args(call.args())
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    for key in [
        "WORKSPACE_ROOT",
        "BOX_DISPLAY_GEOM",
        "BOX_MAX_OWN_SCREENS",
        "BOX_VIEWER_CTL",
    ] {
        if let Some(value) = env_nonempty(key) {
            command.env(key, value);
        }
    }
    command.env(
        "BOX_SHARED_DISPLAY",
        env_nonempty("BOX_DISPLAY").unwrap_or_else(|| ":1".into()),
    );
    match tokio::time::timeout(SCREEN_PATIENCE, command.output()).await {
        Ok(Ok(out)) => out.stdout,
        _ => br#"{"error":"box-screen didn't finish"}"#.to_vec(),
    }
}

/// Lines from the fifo, on a thread: holding it open for writing too means it never reads EOF.
fn listen(path: PathBuf) -> mpsc::UnboundedReceiver<String> {
    let (lines, receiver) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let Ok(file) = OpenOptions::new().read(true).write(true).open(&path) else {
            return;
        };
        let mut reader = BufReader::new(file);
        while let Ok(Some(line)) = next_line(&mut reader) {
            if lines.send(line).is_err() {
                return;
            }
        }
    });
    receiver
}

/// True once the daemons can start: the display takes connections, the tunnel has unlinked its
/// bearer, so that no exec child can ever exist while a staged copy does, and the browser's proxy
/// listens. Its ports are above the box's port floor: with no command able to run yet, whatever
/// listens on its admin port is the proxy, and a command can never take a port of its first.
fn desktop_is_up(features: Features, display: &DesktopConfig, egress_file: Option<&Path>) -> bool {
    let display_up = !features.desktop || UnixStream::connect(display.x11_socket_path()).is_ok();
    display_up && !egress_file.is_some_and(Path::exists) && (!features.proxy || proxy_listens())
}

fn proxy_listens() -> bool {
    let admin = env_nonempty("BOX_EGRESS_ADMIN_BIND").unwrap_or_else(|| "127.0.0.1:8792".into());
    admin.parse::<std::net::SocketAddr>().is_ok_and(|addr| {
        std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok()
    })
}

/// Start a daemon on its staged files, then wait until it has unlinked them. `box-exec` goes last,
/// so no exec child can ever exist while a staged copy does. A copy still there after the wait is
/// a secret on disk nobody is going to clear, so the box does not go on.
async fn start_daemon(
    name: &str,
    caps: &str,
    display: &str,
    env: &[(&str, &Path)],
) -> Result<Child, String> {
    let mut child = service(name, caps)
        .envs(env.iter().copied())
        .env("DISPLAY", display)
        .env("BOX_SHARED_DISPLAY", display)
        .stdin(Stdio::null())
        .spawn()
        .map_err(|err| format!("{name}: {err}"))?;
    let deadline = Instant::now() + UNLINK_PATIENCE;
    while env.iter().any(|(_, path)| path.exists()) {
        let exited = matches!(child.try_wait(), Ok(Some(_)));
        if exited || Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{name} did not take its secrets; stopping the box"));
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    Ok(child)
}

async fn stop(mut children: Vec<Child>) {
    children.iter().for_each(terminate);
    let deadline = Instant::now() + STOP_PATIENCE;
    while Instant::now() < deadline {
        children.retain_mut(|child| !matches!(child.try_wait(), Ok(Some(_))));
        if children.is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    for child in &mut children {
        let _ = child.kill();
        let _ = child.wait();
    }
}

struct Daemons {
    token: String,
    host_token: String,
    viewer_password: Option<String>,
    staging: Staging,
}

impl Daemons {
    /// `box-host`, then `box-exec`. Everything staged is gone when this returns.
    async fn start(self, display: &str) -> Result<Vec<(&'static str, Child)>, String> {
        let stage = &self.staging;
        let host_files = [
            ("BOX_TOKEN_FILE", stage.stage("host.token", &self.token)?),
            (
                "BOX_HOST_TOKEN_FILE",
                stage.stage("host.host_token", &self.host_token)?,
            ),
        ];
        let mut exec_files = vec![
            ("BOX_TOKEN_FILE", stage.stage("exec.token", &self.token)?),
            (
                "BOX_HOST_TOKEN_FILE",
                stage.stage("exec.host_token", &self.host_token)?,
            ),
        ];
        if let Some(password) = &self.viewer_password {
            exec_files.push((
                "BOX_VIEWER_PASSWORD_FILE",
                stage.stage("exec.viewer", password)?,
            ));
        }
        let host_env: Vec<(&str, &Path)> =
            host_files.iter().map(|(k, v)| (*k, v.as_path())).collect();
        let exec_env: Vec<(&str, &Path)> =
            exec_files.iter().map(|(k, v)| (*k, v.as_path())).collect();
        let host = start_daemon("box-host", HOST_CAPS, display, &host_env).await?;
        let exec = match start_daemon("box-exec", EXEC_CAPS, display, &exec_env).await {
            Ok(exec) => exec,
            Err(err) => {
                stop(vec![host]).await;
                return Err(err);
            }
        };
        Ok(vec![("box-host", host), ("box-exec", exec)])
    }
}

/// The box made ready, as root, before anything of it runs. It doesn't start without this.
fn prepare() -> Result<(), String> {
    // SAFETY: geteuid takes nothing and can't fail.
    if unsafe { libc::geteuid() } != 0 {
        return Err("box-keeper runs as root: it starts each part as its own user".into());
    }
    let status = Command::new(PREPARE)
        .stdin(Stdio::null())
        .status()
        .map_err(|err| format!("{PREPARE}: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{PREPARE} failed ({status})"))
    }
}

async fn run() -> Result<i32, String> {
    let features = Features::from_env();
    // as a malformed numeric BOX_* is, before anything is read or made: unset or empty is :1,
    // anything else must be a display name, or the box's paths would name slot 1 while every
    // check refuses the shared display
    let display_config = DesktopConfig::from_env();
    let display = display_config.display.clone();
    let main_slot = display_number(&display)
        .ok_or_else(|| format!("BOX_DISPLAY is not a display name (:N): {display:?}"))?;
    let Secrets {
        token,
        host_token,
        viewer_password,
        egress_bearer,
    } = Secrets::load(features)?;
    prepare()?;
    if features.proxy {
        stage_rules()?;
    }
    let staging = Staging::new("secrets", SVC_UID)?;
    let ctl = PathBuf::from(env_nonempty("BOX_VIEWER_CTL").unwrap_or_else(|| DEFAULT_CTL.into()));
    make_fifo(&ctl)?;
    let mut requests = listen(ctl);
    let screen_requests = screen_socket()?;
    let screen_slots = std::sync::Arc::new(tokio::sync::Semaphore::new(SCREEN_REQUESTS));

    let mut desktop = Command::new(SETPRIV);
    desktop
        .args([
            format!("--reuid={DESKTOP_UID}"),
            format!("--regid={DESKTOP_UID}"),
            DESKTOP_GROUPS.to_string(),
            "--inh-caps=-all".to_string(),
            ENTRYPOINT.to_string(),
        ])
        .stdin(Stdio::null())
        .current_dir(DESKTOP_HOME)
        .env("HOME", DESKTOP_HOME)
        .env("XAUTHORITY", format!("/run/box/x/{main_slot}/auth"))
        .env("BOX_SCREEN_RUN", format!("/run/box/x/{main_slot}/run"))
        .env("BOX_DESKTOP", flag(features.desktop))
        .env("BOX_EGRESS_TUNNEL", flag(features.egress))
        .env("BOX_EGRESS_PROXY", flag(features.proxy));
    let egress_staging = match egress_bearer.as_deref().filter(|_| features.egress) {
        Some(_) => Some(Staging::new("egress-secret", DESKTOP_UID)?),
        None => None,
    };
    let egress_file = match (&egress_staging, egress_bearer.as_deref()) {
        (Some(staging), Some(bearer)) => Some(staging.stage("egress", bearer)?),
        _ => None,
    };
    drop(egress_bearer);
    if let Some(path) = &egress_file {
        desktop.env("BOX_EGRESS_BEARER_STAGED", path);
    }
    let mut desktop = desktop
        .spawn()
        .map_err(|err| format!("{ENTRYPOINT}: {err}"))?;

    let mut viewers = Viewers {
        password: viewer_password.clone().filter(|_| features.desktop),
        main: main_slot,
        slots: slots_dir(),
        running: HashMap::new(),
        leaving: Vec::new(),
    };
    let mut waiting = Some(Daemons {
        token,
        host_token,
        viewer_password,
        staging,
    });
    let mut daemons: Vec<(&str, Child)> = Vec::new();

    let mut terminate_signal = signal(SignalKind::terminate()).map_err(|err| err.to_string())?;
    let mut interrupt_signal = signal(SignalKind::interrupt()).map_err(|err| err.to_string())?;
    let mut tick = tokio::time::interval(TICK);

    let code = loop {
        tokio::select! {
            _ = terminate_signal.recv() => break 0,
            _ = interrupt_signal.recv() => break 0,
            Some(line) = requests.recv() => match Request::parse(&line) {
                Some(Request::Up(slot)) => viewers.up(slot),
                Some(Request::Down(slot)) => viewers.down(slot),
                None => {}
            },
            Ok((stream, _)) = screen_requests.accept() => {
                match screen_slots.clone().try_acquire_owned() {
                    Ok(slot) => {
                        tokio::spawn(async move {
                            serve_screen_request(stream).await;
                            drop(slot);
                        });
                    }
                    Err(_) => {
                        tokio::spawn(refuse_screen_request(stream));
                    }
                }
            },
            _ = tokio::time::sleep(START_POLL), if waiting.is_some() => {
                if desktop_is_up(features, &display_config, egress_file.as_deref()) {
                    viewers.up(main_slot);
                    if let Some(ready) = waiting.take() {
                        match ready.start(&display).await {
                            Ok(started) => daemons = started,
                            Err(err) => {
                                eprintln!("{err}");
                                break 1;
                            }
                        }
                    }
                }
            },
            _ = tick.tick() => {
                viewers.tick();
                if let Ok(Some(status)) = desktop.try_wait() {
                    eprintln!("the desktop exited ({status}); shutting down");
                    break 1;
                }
                let gone = daemons.iter_mut().find_map(|(name, child)| {
                    matches!(child.try_wait(), Ok(Some(_))).then_some(*name)
                });
                if let Some(name) = gone {
                    eprintln!("{name} exited; shutting down");
                    break 1;
                }
            }
        }
    };

    let mut children: Vec<Child> = daemons.into_iter().map(|(_, child)| child).collect();
    children.extend(viewers.running.drain().filter_map(|(_, v)| v.child));
    children.append(&mut viewers.leaving);
    children.push(desktop);
    stop(children).await;
    Ok(code)
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    make_undumpable();
    let code = run().await.unwrap_or_else(|err| {
        eprintln!("{err}");
        1
    });
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::starting_rules;

    #[test]
    fn a_box_with_no_rules_file_starts_with_none_whichever_the_exit() {
        // nothing staged, for a box with the tunnel or the proxy alone: no rules, no site
        assert!(starting_rules(None).is_none());
        assert!(starting_rules(Some(Err("gone".into()))).is_none());
        assert!(starting_rules(Some(Ok(
            br#"{"v":1,"exit":"tunnel","mode":"all","block":["*"]}"#.to_vec()
        )))
        .is_none());
        let rules = starting_rules(Some(Ok(
            br#"{"v":1,"version":3,"exit":"tunnel","mode":"all"}"#.to_vec(),
        )))
        .unwrap();
        assert_eq!(rules.version, 3);
    }
}
