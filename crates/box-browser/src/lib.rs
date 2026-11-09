//! Each screen's Chromium, driven over its DevTools pipe through ultra-instinct, for computer use:
//! read a page as its controls with `@eN` refs, act on one by ref through ultra-instinct's
//! ticketed guard, navigate, use tabs, wait, take a picture, save or load a login, or let
//! Instinct run a goal until it's done, blocked or abstains.
//!
//! - [`seam`]: the only part that knows ultra-instinct's types, and where its pipe transport plugs in.
//! - [`pipe`]: how `box-exec` takes a screen's end of the pipe from its launcher.
//! - [`screen`]: one thread per screen, which owns that screen's pipe and tools (they are not
//!   `Send`), and every operation, run there.
//! - [`shape`]: ultra-instinct's answers, cut down to this crate's own shapes, within limits.
//! - [`policy`]: the confirm and allow-list rules box-browser checks itself, before ultra-instinct's
//!   guard checks them again.
//!
//! Every call has a time limit, every answer a size limit, and a goal run a step and time bound. A
//! ref is good only for its screen's latest snapshot of the current tab. A password field's text is
//! never returned.

pub mod pipe;
pub mod policy;
pub mod screen;
pub mod seam;
pub mod shape;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use policy::Rules;
pub use screen::Screen;
pub use seam::Tools;

/// What a call can fail with. `box-exec` maps each onto a status and a code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BrowserError {
    /// `BOX_BROWSER`, `BOX_CHROME` or `BOX_DESKTOP` is off.
    #[error("the browser layer is off on this computer")]
    Disabled,
    /// The screen has no browser box-exec can use now.
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    Invalid(String),
    /// A ref from an older snapshot, or an element that's gone.
    #[error("{0}")]
    Stale(String),
    /// The press needs a person to confirm `label`; `token` goes back as `confirm`.
    #[error("pressing {label:?} needs a person's confirm: send the token back as `confirm`")]
    ConfirmRequired { label: String, token: String },
    /// The page, or the address asked for, isn't on the call's allow-list.
    #[error("{0} is not on this call's allow_domains")]
    DomainNotAllowed(String),
    /// ultra-instinct's guard refused: the page drifted, or the target is covered or disabled.
    #[error("{0}")]
    Refused(String),
    /// The tab the call was for is gone: list the tabs.
    #[error("{0}")]
    TargetGone(String),
    /// A JavaScript dialog is open on the tab and holds everything else up.
    #[error("{0}")]
    DialogOpen(String),
    /// box-browser asked ultra-instinct for something it may not: a bug here, not the caller's.
    #[error("{0}")]
    Internal(String),
    /// The screen's browser has more calls waiting than it takes.
    #[error("this screen's browser is busy; try again")]
    Busy,
    #[error("{0}")]
    Timeout(String),
    /// The browser answered badly, too much, or not at all.
    #[error("{0}")]
    Backend(String),
}

/// How a screen's tools are made, on that screen's own thread.
pub trait Opener: Send + Sync + 'static {
    fn open(&self, display: &str) -> Result<Box<dyn Tools>, BrowserError>;
}

/// Calls waiting per screen, besides the one running.
const QUEUE: usize = 4;
/// How long one plain call may take, all its tool calls together.
pub const CALL_BUDGET: Duration = Duration::from_secs(45);
/// What a wait or a goal run gets beyond its own timeout.
pub const MARGIN: Duration = Duration::from_secs(15);

type Job = Box<dyn FnOnce(&mut Screen) + Send>;

/// Every screen's browser. Cheap to clone; each screen gets its own thread on first use.
#[derive(Clone)]
pub struct Browsers {
    enabled: bool,
    rules: Arc<Rules>,
    opener: Arc<dyn Opener>,
    threads: Arc<Mutex<HashMap<String, SyncSender<Job>>>>,
    state_root: PathBuf,
}

impl std::fmt::Debug for Browsers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Browsers")
            .field("enabled", &self.enabled)
            .field("confirm_press", &self.rules.globs())
            .field("state_root", &self.state_root)
            .finish()
    }
}

/// Where saved logins go: a folder per screen inside, `box-exec`'s alone (`docker/box-prepare`).
pub const DEFAULT_STATE_ROOT: &str = "/home/box/browser-state";

impl Browsers {
    /// On when `BOX_BROWSER`, `BOX_CHROME` and `BOX_DESKTOP` are, over each screen's real pipe.
    pub fn from_env() -> Self {
        let on = |name: &str| box_common::env_bool(name, true);
        let state_root = std::env::var("BOX_BROWSER_STATE_DIR")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_STATE_ROOT));
        Self::new(
            on("BOX_BROWSER") && on("BOX_CHROME") && on("BOX_DESKTOP"),
            Rules::from_env(),
            Arc::new(pipe::PipeOpener),
            state_root,
        )
    }

    pub fn disabled() -> Self {
        Self::new(
            false,
            Rules::default(),
            Arc::new(pipe::PipeOpener),
            PathBuf::from(DEFAULT_STATE_ROOT),
        )
    }

    pub fn new(enabled: bool, rules: Rules, opener: Arc<dyn Opener>, state_root: PathBuf) -> Self {
        Self {
            enabled,
            rules: Arc::new(rules),
            opener,
            threads: Arc::new(Mutex::new(HashMap::new())),
            state_root,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Runs `work` on `display`'s thread, within `budget`. The thread keeps going past the budget
    /// if the browser does; what waits behind it waits, and past [`QUEUE`] is told it's busy.
    async fn run<R, F>(&self, display: &str, budget: Duration, work: F) -> Result<R, BrowserError>
    where
        R: Send + 'static,
        F: FnOnce(&mut Screen) -> Result<R, BrowserError> + Send + 'static,
    {
        if !self.enabled {
            return Err(BrowserError::Disabled);
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut job: Job = Box::new(move |screen: &mut Screen| {
            let _ = tx.send(work(screen));
        });
        for _ in 0..2 {
            let sender = self.thread(display)?;
            match sender.try_send(job) {
                Ok(()) => {
                    return match tokio::time::timeout(budget, rx).await {
                        Ok(Ok(answer)) => answer,
                        Ok(Err(_)) => Err(BrowserError::Backend(
                            "the screen's browser thread dropped the call".into(),
                        )),
                        Err(_) => Err(BrowserError::Timeout(format!(
                            "the browser did not answer in {} s",
                            budget.as_secs()
                        ))),
                    };
                }
                Err(TrySendError::Full(_)) => return Err(BrowserError::Busy),
                Err(TrySendError::Disconnected(back)) => {
                    // The thread is gone: start another, once.
                    self.lock().remove(display);
                    job = back;
                }
            }
        }
        Err(BrowserError::Backend(
            "the screen's browser thread would not start".into(),
        ))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, SyncSender<Job>>> {
        self.threads
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn thread(&self, display: &str) -> Result<SyncSender<Job>, BrowserError> {
        let mut threads = self.lock();
        if let Some(sender) = threads.get(display) {
            return Ok(sender.clone());
        }
        let (sender, jobs) = sync_channel::<Job>(QUEUE);
        let opener = self.opener.clone();
        let rules = self.rules.clone();
        let name = display.to_string();
        std::thread::Builder::new()
            .name(format!("browser {display}"))
            .spawn(move || screen::serve(Screen::new(name, opener, rules), jobs))
            .map_err(|err| BrowserError::Backend(format!("no thread for the browser: {err}")))?;
        threads.insert(display.to_string(), sender.clone());
        Ok(sender)
    }

    /// Asks every screen's browser to quit and lets go of its pipe, within `grace`: for
    /// box-exec's shutdown. Each launcher ends what's left of its Chromium after that.
    pub async fn close_all(&self, grace: Duration) {
        let senders: Vec<SyncSender<Job>> = self.lock().drain().map(|(_, s)| s).collect();
        let mut waits = Vec::new();
        for sender in senders {
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            let job: Job = Box::new(move |screen: &mut Screen| {
                screen.close();
                let _ = tx.send(());
            });
            if sender.try_send(job).is_ok() {
                waits.push(rx);
            }
        }
        let all = async {
            for wait in waits {
                let _ = wait.await;
            }
        };
        let _ = tokio::time::timeout(grace, all).await;
    }

    pub async fn snap(
        &self,
        display: &str,
        req: SnapRequest,
    ) -> Result<SnapResponse, BrowserError> {
        let limits = req.limits()?;
        self.run(display, CALL_BUDGET, move |s| s.snap(&limits))
            .await
    }

    pub async fn act(&self, display: &str, req: ActRequest) -> Result<ActResponse, BrowserError> {
        req.check()?;
        let allow = policy::AllowList::parse(req.allow_domains.as_deref())?;
        self.run(display, CALL_BUDGET, move |s| s.act(&req, &allow))
            .await
    }

    pub async fn navigate(
        &self,
        display: &str,
        req: NavigateRequest,
    ) -> Result<NavigateResponse, BrowserError> {
        screen::check_url(&req.url)?;
        let allow = policy::AllowList::parse(req.allow_domains.as_deref())?;
        self.run(display, CALL_BUDGET, move |s| s.navigate(&req.url, &allow))
            .await
    }

    pub async fn tabs(
        &self,
        display: &str,
        req: TabsRequest,
    ) -> Result<TabsResponse, BrowserError> {
        req.check()?;
        let allow = policy::AllowList::parse(req.allow_domains.as_deref())?;
        self.run(display, CALL_BUDGET, move |s| s.tabs(&req, &allow))
            .await
    }

    pub async fn wait(
        &self,
        display: &str,
        req: WaitRequest,
    ) -> Result<WaitResponse, BrowserError> {
        let timeout = req.timeout()?;
        self.run(display, timeout + MARGIN, move |s| s.wait(&req, timeout))
            .await
    }

    pub async fn shot(&self, display: &str) -> Result<ShotResponse, BrowserError> {
        self.run(display, CALL_BUDGET, |s| s.shot()).await
    }

    /// `screen` names the screen's folder of saved logins: the Bot holding it, or `main`.
    pub async fn state(
        &self,
        display: &str,
        screen: &str,
        req: StateRequest,
    ) -> Result<StateResponse, BrowserError> {
        let folder = screen::state_folder(&self.state_root, screen)?;
        let name = req.name()?;
        self.run(display, CALL_BUDGET, move |s| {
            s.state(req.op, &folder, &name)
        })
        .await
    }

    pub async fn run_goal(
        &self,
        display: &str,
        req: RunRequest,
    ) -> Result<RunResponse, BrowserError> {
        let bounds = req.bounds()?;
        let allow = policy::AllowList::parse(req.allow_domains.as_deref())?;
        let budget = bounds.1 + MARGIN;
        self.run(display, budget, move |s| s.run_goal(&req, bounds, &allow))
            .await
    }
}

// ---- the wire ----

pub const DEFAULT_MAX_NODES: usize = 200;
pub const MAX_NODES: usize = 1000;
/// The most text `type` and `select` take.
pub const MAX_TEXT_BYTES: usize = 64 * 1024;
pub const DEFAULT_WAIT_MS: u64 = 10_000;
pub const MAX_WAIT_MS: u64 = 60_000;
pub const DEFAULT_MAX_STEPS: u32 = 20;
pub const MAX_STEPS: u32 = 50;
pub const DEFAULT_RUN_MS: u64 = 120_000;
pub const MAX_RUN_MS: u64 = 300_000;
pub const MAX_GOAL_CHARS: usize = 2000;

/// `POST /v1/browser/snap`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapRequest {
    /// The most elements returned (the page's first ones in reading order).
    pub max_nodes: Option<usize>,
}

#[derive(Clone, Copy, Debug)]
pub struct SnapLimits {
    pub max_nodes: usize,
}

impl SnapRequest {
    pub fn limits(&self) -> Result<SnapLimits, BrowserError> {
        let max_nodes = self.max_nodes.unwrap_or(DEFAULT_MAX_NODES);
        if !(1..=MAX_NODES).contains(&max_nodes) {
            return Err(BrowserError::Invalid(format!(
                "max_nodes is 1 to {MAX_NODES}, not {max_nodes}"
            )));
        }
        Ok(SnapLimits { max_nodes })
    }
}

/// A rectangle in the page's viewport, in CSS pixels.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Element {
    /// `@eN`: good for this snapshot alone.
    #[serde(rename = "ref")]
    pub reference: String,
    pub role: String,
    pub label: String,
    pub actions: Vec<String>,
    pub rect: Rect,
    /// A password field, or one that may be: its text is never returned.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub sensitive: bool,
    /// A field's text, only when it's known not to be a password's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SnapResponse {
    /// This snapshot's number: its refs are good while it is the current tab's latest.
    pub snapshot: u64,
    pub display: String,
    pub url: Option<String>,
    pub title: Option<String>,
    pub elements: Vec<Element>,
    /// Interactive elements on the page; more than `elements` when `truncated`.
    pub total: usize,
    pub truncated: bool,
    /// One line per element, `@eN role "label"`, for a model to read.
    pub text: String,
    pub elapsed_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActAction {
    Click,
    Type,
    Select,
    Key,
    Scroll,
    Focus,
}

/// `POST /v1/browser/act`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActRequest {
    /// `@eN` from the latest snap, or `<snapshot>:@eN`.
    #[serde(rename = "ref")]
    pub reference: String,
    /// The snapshot the ref is from; refused as stale unless it's the latest.
    pub snapshot: Option<u64>,
    pub action: ActAction,
    /// `type`: the text; `select`: the option; `key`: the key (`Enter`, `Escape`, `ArrowDown`, …).
    pub text: Option<String>,
    /// The token a `confirm_required` answer gave, once a person approved its label.
    pub confirm: Option<String>,
    pub allow_domains: Option<Vec<String>>,
}

impl ActRequest {
    pub fn check(&self) -> Result<(), BrowserError> {
        let needs_text = matches!(
            self.action,
            ActAction::Type | ActAction::Select | ActAction::Key
        );
        match (&self.text, needs_text) {
            (None, true) => Err(BrowserError::Invalid(format!(
                "{} needs `text`",
                action_name(self.action)
            ))),
            (Some(_), false) => Err(BrowserError::Invalid(format!(
                "`text` is for type, select and key, not {}",
                action_name(self.action)
            ))),
            (Some(text), true) if text.len() > MAX_TEXT_BYTES => Err(BrowserError::Invalid(
                format!("text is at most {MAX_TEXT_BYTES} bytes"),
            )),
            (Some(text), true) if self.action != ActAction::Type && text.trim().is_empty() => Err(
                BrowserError::Invalid(format!("{} needs `text`", action_name(self.action))),
            ),
            _ => Ok(()),
        }?;
        let presses = matches!(
            self.action,
            ActAction::Click | ActAction::Key | ActAction::Type
        );
        if self.confirm.is_some() && !presses {
            return Err(BrowserError::Invalid(
                "`confirm` is for click, key and type alone".into(),
            ));
        }
        Ok(())
    }
}

pub fn action_name(action: ActAction) -> &'static str {
    match action {
        ActAction::Click => "click",
        ActAction::Type => "type",
        ActAction::Select => "select",
        ActAction::Key => "key",
        ActAction::Scroll => "scroll",
        ActAction::Focus => "focus",
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ActResponse {
    pub ok: bool,
    #[serde(rename = "ref")]
    pub reference: String,
    pub action: ActAction,
    /// The guard's one-shot ticket the action ran under.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket: Option<u64>,
}

/// `POST /v1/browser/navigate`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NavigateRequest {
    pub url: String,
    pub allow_domains: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct NavigateResponse {
    pub ok: bool,
    pub url: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TabsOp {
    List,
    Open,
    Close,
    Switch,
}

/// `POST /v1/browser/tabs`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TabsRequest {
    pub op: TabsOp,
    /// `close`, `switch`: the tab, as `list` names it.
    pub tab_id: Option<String>,
    /// `open`: the page (default `about:blank`).
    pub url: Option<String>,
    pub allow_domains: Option<Vec<String>>,
}

impl TabsRequest {
    pub fn check(&self) -> Result<(), BrowserError> {
        match (self.op, &self.tab_id, &self.url) {
            (TabsOp::Close | TabsOp::Switch, None, _) => Err(BrowserError::Invalid(
                "close and switch need `tab_id`".into(),
            )),
            (TabsOp::Close | TabsOp::Switch, Some(id), _)
                if id.trim().is_empty() || id.len() > 128 =>
            {
                Err(BrowserError::Invalid(
                    "`tab_id` is 1 to 128 characters".into(),
                ))
            }
            (TabsOp::List | TabsOp::Open, Some(_), _) => Err(BrowserError::Invalid(
                "`tab_id` is for close and switch".into(),
            )),
            (TabsOp::Open, None, Some(url)) => screen::check_url(url),
            (TabsOp::List | TabsOp::Close | TabsOp::Switch, _, Some(_)) => {
                Err(BrowserError::Invalid("`url` is for open".into()))
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Tab {
    pub tab_id: String,
    pub url: String,
    pub title: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct TabsResponse {
    pub op: TabsOp,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tabs: Option<Vec<Tab>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadState {
    Domcontentloaded,
    Load,
}

/// `POST /v1/browser/wait`. With no condition, it waits out `timeout_ms`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitRequest {
    /// The page's visible text contains this (case-insensitive).
    pub text: Option<String>,
    /// The address contains this.
    pub url: Option<String>,
    /// A control's label contains this (case-insensitive).
    pub control: Option<String>,
    /// No control is labelled exactly this.
    pub gone: Option<String>,
    pub load: Option<LoadState>,
    pub timeout_ms: Option<u64>,
}

pub const MAX_WAIT_TEXT: usize = 1024;

impl WaitRequest {
    pub fn timeout(&self) -> Result<Duration, BrowserError> {
        for (name, part) in [
            ("text", &self.text),
            ("url", &self.url),
            ("control", &self.control),
            ("gone", &self.gone),
        ] {
            if let Some(part) = part {
                if part.trim().is_empty() || part.len() > MAX_WAIT_TEXT {
                    return Err(BrowserError::Invalid(format!(
                        "`{name}` is 1 to {MAX_WAIT_TEXT} bytes"
                    )));
                }
            }
        }
        let ms = self.timeout_ms.unwrap_or(DEFAULT_WAIT_MS);
        if !(1..=MAX_WAIT_MS).contains(&ms) {
            return Err(BrowserError::Invalid(format!(
                "timeout_ms is 1 to {MAX_WAIT_MS}, not {ms}"
            )));
        }
        Ok(Duration::from_millis(ms))
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct WaitResponse {
    /// Every condition held before the time ran out. Always false for a plain wait.
    pub seen: bool,
    pub after_ms: u64,
    pub url: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ShotResponse {
    pub format: &'static str,
    /// The PNG, base64.
    pub data: String,
    pub bytes: usize,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateOp {
    Save,
    Load,
}

/// `POST /v1/browser/state`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateRequest {
    pub op: StateOp,
    /// Which saved login, in this screen's folder: letters, digits, `_` and `-` (default `default`).
    pub name: Option<String>,
}

impl StateRequest {
    pub fn name(&self) -> Result<String, BrowserError> {
        let name = self.name.as_deref().unwrap_or("default");
        let ok = (1..=64).contains(&name.len())
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if !ok {
            return Err(BrowserError::Invalid(
                "a saved login's name is 1 to 64 letters, digits, _ and -".into(),
            ));
        }
        Ok(name.to_string())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct StateResponse {
    pub op: StateOp,
    pub name: String,
    /// Cookies saved or set. Counts only: never a value.
    pub cookies: u64,
    /// `save`: the origins whose storage was saved.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub origins: Vec<String>,
    /// `load`: origins with saved storage the tab isn't on; navigate there and load again.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pending_origins: Vec<String>,
}

/// `POST /v1/browser/run`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRequest {
    pub goal: String,
    pub max_steps: Option<u32>,
    pub timeout_ms: Option<u64>,
    pub allow_domains: Option<Vec<String>>,
    /// The token a `blocked` run gave, once a person approved its `confirm_label`.
    pub confirm: Option<String>,
}

impl RunRequest {
    pub fn bounds(&self) -> Result<(u32, Duration), BrowserError> {
        let chars = self.goal.chars().count();
        if self.goal.trim().is_empty() || chars > MAX_GOAL_CHARS {
            return Err(BrowserError::Invalid(format!(
                "a goal is 1 to {MAX_GOAL_CHARS} characters"
            )));
        }
        let steps = self.max_steps.unwrap_or(DEFAULT_MAX_STEPS);
        if !(1..=MAX_STEPS).contains(&steps) {
            return Err(BrowserError::Invalid(format!(
                "max_steps is 1 to {MAX_STEPS}, not {steps}"
            )));
        }
        let ms = self.timeout_ms.unwrap_or(DEFAULT_RUN_MS);
        if !(1000..=MAX_RUN_MS).contains(&ms) {
            return Err(BrowserError::Invalid(format!(
                "timeout_ms is 1000 to {MAX_RUN_MS}, not {ms}"
            )));
        }
        Ok((steps, Duration::from_millis(ms)))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Done,
    Blocked,
    Abstained,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Step {
    pub action: String,
    pub target: String,
    pub effect: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct RunResponse {
    pub outcome: Outcome,
    /// `blocked`: why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// `blocked` on a confirm: the label a person may approve.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirm_label: Option<String>,
    /// `blocked` on a confirm: the token to send back as `confirm`, once they have.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirm: Option<String>,
    /// `blocked`: `needs_confirm` (with `confirm_label` and `confirm`) or `domain_not_allowed`
    /// (with `host`), the codes a press's error would carry. None: the run stopped itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<&'static str>,
    /// `blocked` on `domain_not_allowed`: the host off the list, when it could be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// `abstained`, `failed`: why. After an abstain the caller's model may take over.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    /// Every verified step, in order.
    pub steps: Vec<Step>,
    pub elapsed_ms: u64,
}

#[cfg(test)]
mod tests;
