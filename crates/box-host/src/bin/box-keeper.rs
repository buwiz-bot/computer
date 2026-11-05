//! `box-keeper`: the one process in the box that reads the secrets, and the parent of the rest.
//!
//! Everything in the box runs as one user, so any process that holds a secret and can be dumped
//! gives it to any exec child through `/proc/<pid>/mem`. A shell cannot be made safe that way,
//! so `entrypoint.sh` never sees a secret. This process reads them, and it is not dumpable: its
//! binary is execute-only and it sets `PR_SET_DUMPABLE` to 0, as `box-exec`, `box-host`,
//! `box-egress-tunnel` and voxl do in their own way.
//!
//! It starts `entrypoint.sh` for the desktop, hands `box-host` and `box-exec` what each needs in
//! a staged file they unlink, and keeps one voxl per screen. voxl takes its password from argv or
//! its environment only, so it gets it in its environment, which only it can read.

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use box_common::{env_bool, env_nonempty, make_undumpable, read_secret_from_env};
use box_desktop::{display_number, slots_dir, VIEWER_PORT_BASE};
use box_host::keeper::{may_have_viewer, restart_delay, Request, STABLE};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc;

const ENTRYPOINT: &str = "/usr/local/bin/entrypoint.sh";
const DEFAULT_CTL: &str = "/tmp/box-viewer.ctl";
const SECRET_DIR_PREFIX: &str = "box-secrets.";
const TICK: Duration = Duration::from_millis(200);
const STOP_PATIENCE: Duration = Duration::from_secs(8);
/// How long a daemon gets to read and unlink what was staged for it.
const UNLINK_PATIENCE: Duration = Duration::from_secs(3);

struct Secrets {
    token: String,
    host_token: String,
    viewer_password: Option<String>,
    egress_bearer: Option<String>,
}

/// Read one secret. A mount that is read-only keeps its copy readable to the box user, and that
/// is worth saying out loud at every start.
fn secret(name: &str) -> Result<Option<String>, String> {
    let path = env_nonempty(&format!("{name}_FILE"));
    if path.is_none() && env_nonempty(name).is_some() {
        eprintln!("{name} came as a value: it stays readable in /proc/1/environ. Use {name}_FILE.");
    }
    let value = read_secret_from_env(name, true).map_err(|err| err.to_string())?;
    if let Some(path) = path.filter(|path| Path::new(path).exists()) {
        eprintln!(
            "{path} could not be unlinked: anything running as the box user can read it. \
             Stage it on a writable mount so it is gone once the box is up."
        );
    }
    Ok(value)
}

impl Secrets {
    fn load() -> Result<Self, String> {
        let token = secret("BOX_TOKEN")?.ok_or("BOX_TOKEN or BOX_TOKEN_FILE must be set")?;
        let host_token = secret("BOX_HOST_TOKEN")?.unwrap_or_else(|| token.clone());
        let viewer_password = secret("BOX_VIEWER_PASSWORD")?;
        if env_bool("BOX_DESKTOP", true) && viewer_password.is_none() {
            return Err(
                "BOX_VIEWER_PASSWORD or BOX_VIEWER_PASSWORD_FILE must be set when BOX_DESKTOP=1 \
                 (independent of BOX_TOKEN)"
                    .into(),
            );
        }
        let egress_bearer = secret("BOX_EGRESS_TUNNEL_BEARER")?;
        if env_bool("BOX_EGRESS_TUNNEL", false) && egress_bearer.is_none() {
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
}

impl Staging {
    fn new() -> Result<Self, String> {
        // A killed box leaves its directory behind, and nothing else makes one of these.
        if let Ok(entries) = fs::read_dir("/tmp") {
            for entry in entries.flatten() {
                if entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(SECRET_DIR_PREFIX)
                {
                    let _ = fs::remove_dir_all(entry.path());
                }
            }
        }
        let dir = PathBuf::from(format!("/tmp/{SECRET_DIR_PREFIX}{}", std::process::id()));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|err| format!("secret staging: {err}"))?;
        Ok(Self { dir })
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
        Ok(path)
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

struct Viewer {
    child: Option<Child>,
    started: Instant,
    failures: u32,
    retry_at: Instant,
}

struct Viewers {
    password: Option<String>,
    main: u16,
    slots: PathBuf,
    running: HashMap<u16, Viewer>,
    /// Told to stop, and not yet reaped.
    leaving: Vec<Child>,
}

impl Viewers {
    fn spawn(&self, slot: u16) -> std::io::Result<Child> {
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(format!("/tmp/voxl-{slot}.log"))?;
        Command::new("voxl")
            .env("DISPLAY", format!(":{slot}"))
            .env(
                "VOXL_PASSWORD",
                self.password.as_deref().unwrap_or_default(),
            )
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
        if self.running.contains_key(&slot) || !may_have_viewer(slot, self.main, &self.slots) {
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
            if due {
                match self.spawn(slot) {
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

/// Lines from the fifo, on a thread: holding it open for writing too means it never reads EOF.
fn listen(path: PathBuf) -> mpsc::UnboundedReceiver<String> {
    let (lines, receiver) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let Ok(file) = OpenOptions::new().read(true).write(true).open(&path) else {
            return;
        };
        let mut reader = BufReader::new(file);
        loop {
            let mut line = Vec::new();
            match reader.by_ref().take(64).read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    let text = String::from_utf8_lossy(&line).into_owned();
                    if lines.send(text).is_err() {
                        return;
                    }
                }
            }
        }
    });
    receiver
}

/// Start a daemon on its staged files, then wait until it has unlinked them. `box-exec` goes last,
/// so no exec child can ever exist while a staged copy does.
async fn start_daemon(name: &str, display: &str, env: &[(&str, &Path)]) -> Result<Child, String> {
    let home = env_nonempty("HOME").unwrap_or_else(|| "/home/box".into());
    let child = Command::new(name)
        .envs(env.iter().copied())
        // What the entrypoint sets for the desktop, for the daemons and their exec children.
        .env("DISPLAY", display)
        .env("BOX_SHARED_DISPLAY", display)
        .env("XDG_RUNTIME_DIR", "/tmp/runtime-box")
        .env("XDG_CONFIG_HOME", format!("{home}/.config"))
        .stdin(Stdio::null())
        .spawn()
        .map_err(|err| format!("{name}: {err}"))?;
    let deadline = Instant::now() + UNLINK_PATIENCE;
    while env.iter().any(|(_, path)| path.exists()) && Instant::now() < deadline {
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

async fn run() -> Result<i32, String> {
    let secrets = Secrets::load()?;
    let Secrets {
        token,
        host_token,
        viewer_password,
        egress_bearer,
    } = secrets;
    let staging = Staging::new()?;
    let ctl = PathBuf::from(env_nonempty("BOX_VIEWER_CTL").unwrap_or_else(|| DEFAULT_CTL.into()));
    make_fifo(&ctl)?;
    let mut requests = listen(ctl);

    let mut desktop = Command::new(ENTRYPOINT);
    desktop.stdin(Stdio::null());
    if let Some(bearer) = &egress_bearer {
        desktop.env("BOX_EGRESS_BEARER_STAGED", staging.stage("egress", bearer)?);
    }
    drop(egress_bearer);
    let mut desktop = desktop
        .spawn()
        .map_err(|err| format!("{ENTRYPOINT}: {err}"))?;

    let display = env_nonempty("BOX_DISPLAY").unwrap_or_else(|| ":1".into());
    let mut viewers = Viewers {
        password: viewer_password.clone(),
        main: display_number(&display).unwrap_or(1),
        slots: slots_dir(),
        running: HashMap::new(),
        leaving: Vec::new(),
    };
    let mut secrets = Some((token, host_token, viewer_password));
    let mut staging = Some(staging);
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
                Some(Request::Start) => {
                    if let (Some((token, host_token, password)), Some(stage)) =
                        (secrets.take(), staging.as_ref())
                    {
                        let host_files = [
                            ("BOX_TOKEN_FILE", stage.stage("host.token", &token)?),
                            ("BOX_HOST_TOKEN_FILE", stage.stage("host.host_token", &host_token)?),
                        ];
                        let mut exec_files = vec![
                            ("BOX_TOKEN_FILE", stage.stage("exec.token", &token)?),
                            ("BOX_HOST_TOKEN_FILE", stage.stage("exec.host_token", &host_token)?),
                        ];
                        if let Some(password) = &password {
                            exec_files.push((
                                "BOX_VIEWER_PASSWORD_FILE",
                                stage.stage("exec.viewer", password)?,
                            ));
                        }
                        let host_env: Vec<(&str, &Path)> =
                            host_files.iter().map(|(k, v)| (*k, v.as_path())).collect();
                        let exec_env: Vec<(&str, &Path)> =
                            exec_files.iter().map(|(k, v)| (*k, v.as_path())).collect();
                        daemons.push(("box-host", start_daemon("box-host", &display, &host_env).await?));
                        daemons.push(("box-exec", start_daemon("box-exec", &display, &exec_env).await?));
                        staging = None;
                    }
                }
                None => {}
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
