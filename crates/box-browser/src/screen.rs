//! One screen's browser, on a thread of its own. It owns the screen's tools, and so its pipe, for
//! as long as they work, and runs every call for the screen in turn. Nothing of it is shared with
//! another thread: calls come in as jobs, answers go back on a channel.

use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde_json::{json, Value};

use crate::policy::{AllowList, Confirms, Rules};
use crate::seam::{GoalBounds, GoalEnd, PolicyLists, ToolFailure, Tools};
use crate::shape::{self, Seen};
use crate::{
    ActAction, ActRequest, ActResponse, BrowserError, Element, Job, NavigateResponse, Opener,
    Outcome, RunRequest, RunResponse, ShotResponse, SnapLimits, SnapResponse, StateOp,
    StateResponse, Step, TabsOp, TabsRequest, TabsResponse, WaitRequest, WaitResponse,
};

/// The most a screenshot may be, decoded.
pub const MAX_SHOT_BYTES: usize = 8 * 1024 * 1024;
/// The most any other answer may be, as JSON.
pub const MAX_ANSWER_BYTES: usize = 4 * 1024 * 1024;
/// Steps of a goal run returned.
const MAX_REPORTED_STEPS: usize = 50;
pub const MAX_URL_BYTES: usize = 8 * 1024;

/// Snapshot numbers start somewhere new each time box-exec does, as box-a11y's do, so a ref kept
/// from before a restart is very unlikely to name an element of a new snapshot.
static SNAPSHOTS: AtomicU64 = AtomicU64::new(0);

fn next_snapshot() -> u64 {
    if SNAPSHOTS.load(Ordering::Relaxed) == 0 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let seed = (now.subsec_nanos() as u64 ^ ((std::process::id() as u64) << 7)) % 900_000;
        let _ = SNAPSHOTS.compare_exchange(0, seed + 1000, Ordering::Relaxed, Ordering::Relaxed);
    }
    SNAPSHOTS.fetch_add(1, Ordering::Relaxed) + 1
}

/// The elements of the current tab's latest snapshot, by their `@eN`.
#[derive(Clone, Debug)]
struct Snapshot {
    id: u64,
    elements: Vec<(usize, String, String)>,
}

pub struct Screen {
    display: String,
    opener: Arc<dyn Opener>,
    rules: Arc<Rules>,
    tools: Option<Box<dyn Tools>>,
    /// The policy last given to the tools, so it's set again only when it changes.
    policy: Option<PolicyLists>,
    latest: Option<Snapshot>,
    confirms: Confirms,
}

/// The thread: one job at a time, until box-exec drops the sender.
pub(crate) fn serve(mut screen: Screen, jobs: Receiver<Job>) {
    while let Ok(job) = jobs.recv() {
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(&mut screen)));
        if ran.is_err() {
            // What the call was in the middle of is unknown, so its snapshot and policy go. The
            // pipe stays: Chromium quits, with status 0, as soon as box-exec's end closes, and the
            // restart loop takes a 0 for a person closing it, so dropping the pipe here would close
            // the screen's browser for good.
            screen.forget();
        }
    }
}

impl Screen {
    pub fn new(display: String, opener: Arc<dyn Opener>, rules: Arc<Rules>) -> Self {
        Self {
            display,
            opener,
            rules,
            tools: None,
            policy: None,
            latest: None,
            confirms: Confirms::default(),
        }
    }

    /// The pipe is gone, or is being let go: so are the tools on it.
    fn reset(&mut self) {
        self.tools = None;
        self.forget();
    }

    /// Everything but the pipe.
    fn forget(&mut self) {
        self.policy = None;
        self.latest = None;
    }

    fn tools(&mut self) -> Result<&mut Box<dyn Tools>, BrowserError> {
        if self.tools.is_none() {
            self.tools = Some(self.opener.open(&self.display)?);
            self.policy = None;
        }
        Ok(self.tools.as_mut().expect("just opened"))
    }

    /// A tool's failure as a box-browser error. A closed pipe drops the tools, so the next call
    /// opens the screen's new browser.
    fn failed(&mut self, failure: ToolFailure, target: Option<&str>) -> BrowserError {
        match failure {
            ToolFailure::Invalid(m) => BrowserError::Invalid(m),
            ToolFailure::NeedsConfirm { label } => match self.confirms.issue(&label, target) {
                Ok(token) => BrowserError::ConfirmRequired { label, token },
                Err(err) => err,
            },
            ToolFailure::DomainNotAllowed(host) => {
                BrowserError::DomainNotAllowed(if host.is_empty() {
                    "the page".into()
                } else {
                    host
                })
            }
            ToolFailure::Refused(m) => BrowserError::Refused(m),
            ToolFailure::Closed(m) => {
                self.reset();
                BrowserError::Unavailable(format!("the browser went away: {m}"))
            }
            ToolFailure::Unavailable(m) => BrowserError::Unavailable(m),
            ToolFailure::Stale(m) => BrowserError::Stale(m),
            ToolFailure::TargetGone(m) => {
                self.latest = None;
                BrowserError::TargetGone(m)
            }
            ToolFailure::DialogOpen(m) => BrowserError::DialogOpen(m),
            ToolFailure::Timeout(m) => BrowserError::Timeout(m),
            // only this call's answer is lost: the pipe and the other tabs go on
            ToolFailure::TooLarge(m) => BrowserError::Backend(m),
            ToolFailure::NotAllowed(m) => BrowserError::Internal(m),
            ToolFailure::Browser(m) => BrowserError::Backend(m),
        }
    }

    /// One tool call, its answer held to `cap` bytes of JSON.
    fn call(
        &mut self,
        name: &str,
        args: Value,
        target: Option<&str>,
    ) -> Result<Value, BrowserError> {
        let cap = if name == "browser_screenshot" {
            MAX_SHOT_BYTES / 3 * 4 + 4096
        } else {
            MAX_ANSWER_BYTES
        };
        let answer = self.tools()?.call(name, &args);
        let answer = answer.map_err(|f| self.failed(f, target))?;
        let size = serde_json::to_vec(&answer)
            .map(|v| v.len())
            .unwrap_or(usize::MAX);
        if size > cap {
            return Err(BrowserError::Backend(format!(
                "the browser's answer to {name} is {size} bytes, more than {cap}"
            )));
        }
        Ok(answer)
    }

    /// Gives ultra-instinct the call's policy, when it isn't the one it has.
    fn apply(&mut self, allow: &AllowList, confirmed: Vec<String>) -> Result<(), BrowserError> {
        let lists = PolicyLists {
            allow_domains: allow.raw().to_vec(),
            confirm_press: self.rules.globs().to_vec(),
            confirmed,
        };
        if self.policy.as_ref() == Some(&lists) {
            return Ok(());
        }
        let set = self.tools()?.set_policy(&lists);
        set.map_err(|f| self.failed(f, None))?;
        self.policy = Some(lists);
        Ok(())
    }

    fn look(&mut self) -> Result<shape::Page, BrowserError> {
        let answer = self.call("browser_get_state", json!({"format": "json"}), None)?;
        shape::page(&answer)
    }

    pub fn snap(&mut self, limits: &SnapLimits) -> Result<SnapResponse, BrowserError> {
        let started = Instant::now();
        let page = self.look()?;
        let kept: Vec<&Seen> = page.elements.iter().take(limits.max_nodes).collect();
        let truncated = kept.len() < page.elements.len() || page.total > page.elements.len();
        let id = next_snapshot();
        let elements: Vec<Element> = kept
            .iter()
            .map(|e| Element {
                reference: format!("@e{}", e.index),
                role: e.role.clone(),
                label: e.label.clone(),
                actions: e.actions.clone(),
                rect: e.rect,
                sensitive: e.sensitive,
                value: e.value.clone(),
            })
            .collect();
        self.latest = Some(Snapshot {
            id,
            elements: kept
                .iter()
                .map(|e| (e.index, e.region.clone(), e.label.clone()))
                .collect(),
        });
        Ok(SnapResponse {
            snapshot: id,
            display: self.display.clone(),
            url: page.url,
            title: page.title,
            text: shape::render(&elements),
            total: page.total.max(page.elements.len()),
            truncated,
            elements,
            elapsed_ms: started.elapsed().as_millis() as u64,
        })
    }

    pub fn act(
        &mut self,
        req: &ActRequest,
        allow: &AllowList,
    ) -> Result<ActResponse, BrowserError> {
        let (n, named) = parse_ref(&req.reference)?;
        let latest = self.latest.clone().ok_or_else(|| {
            BrowserError::Stale(format!(
                "no snapshot of {}'s current tab to act on: take one with POST /v1/browser/snap",
                self.display
            ))
        })?;
        if let Some(id) = req.snapshot.or(named) {
            if id != latest.id {
                return Err(BrowserError::Stale(format!(
                    "{} is from snapshot {id}, and the latest is {}: snap again",
                    req.reference.trim(),
                    latest.id
                )));
            }
        }
        let (_, region, snapped_label) = latest
            .elements
            .iter()
            .find(|(index, _, _)| *index == n)
            .cloned()
            .ok_or_else(|| BrowserError::Invalid(format!("snapshot {} has no @e{n}", latest.id)))?;

        // The page as it is now: the address for the allow-list, and where the element went.
        let now = self.look()?;
        allow.check(now.url.as_deref())?;
        let here = now
            .elements
            .iter()
            .find(|e| e.region == region)
            .cloned()
            .ok_or_else(|| {
                BrowserError::Stale(format!("@e{n} is gone from the page: snap again"))
            })?;

        let mut confirmed = Vec::new();
        let label = here.label.clone();
        if req.action == ActAction::Click
            && (self.rules.needs_confirm(&label) || self.rules.needs_confirm(&snapped_label))
        {
            let redeemed = req
                .confirm
                .as_deref()
                .and_then(|token| self.confirms.redeem(token, Some(&region)));
            match redeemed {
                // A token confirms the label it was given for, and nothing it has turned into.
                Some(approved) if approved == label => confirmed.push(approved),
                _ => {
                    let token = self.confirms.issue(&label, Some(&region))?;
                    return Err(BrowserError::ConfirmRequired { label, token });
                }
            }
        }
        self.apply(allow, confirmed.clone())?;

        let index = here.index;
        let snapshot = now.snapshot.clone().ok_or_else(|| {
            BrowserError::Backend("the page's state came with no snapshot to press in".into())
        })?;
        let (tool, mut args) = match req.action {
            ActAction::Click => {
                let mut args = json!({"index": index});
                if let Some(label) = confirmed.first() {
                    args["confirm"] = json!(label);
                }
                ("browser_click", args)
            }
            ActAction::Type => ("browser_type", json!({"index": index, "text": req.text})),
            ActAction::Select => (
                "browser_select_dropdown",
                json!({"index": index, "text": req.text}),
            ),
            ActAction::Key => ("browser_send_key", json!({"index": index, "key": req.text})),
            ActAction::Scroll => ("browser_scroll_to", json!({"index": index})),
            ActAction::Focus => ("browser_focus", json!({"index": index})),
        };
        // the press is bound to the element this look showed: one that moved or changed since is
        // refused `stale_ref`, with nothing pressed
        args["snapshot"] = snapshot;
        let done = self.call(tool, args, Some(&region));
        if !confirmed.is_empty() {
            // The confirm was for this press alone: the next call gets the policy without it.
            self.policy = None;
        }
        let done = done?;
        // ultra-instinct finds the index again on its own fresh look. If that was another element,
        // the page moved under the call, and the caller must know, not be told it went well.
        if let Some(acted_on) = done.get("region").and_then(Value::as_str) {
            if acted_on != region {
                self.latest = None;
                return Err(BrowserError::Stale(format!(
                    "the page moved as @e{n} was acted on, and another element took it: snap again"
                )));
            }
        }
        Ok(ActResponse {
            ok: true,
            reference: format!("@e{n}"),
            action: req.action,
            ticket: done.get("ticket").and_then(Value::as_u64),
        })
    }

    pub fn navigate(
        &mut self,
        url: &str,
        allow: &AllowList,
    ) -> Result<NavigateResponse, BrowserError> {
        allow.check(Some(url))?;
        self.apply(allow, Vec::new())?;
        self.latest = None;
        self.call("browser_navigate", json!({"url": url}), None)?;
        Ok(NavigateResponse {
            ok: true,
            url: url.to_string(),
        })
    }

    /// The tab tools, over the pipe's own `Target.*`. The call's policy goes first: a new tab is a
    /// navigation, and ultra-instinct checks it against the allow-list too.
    pub fn tabs(
        &mut self,
        req: &TabsRequest,
        allow: &AllowList,
    ) -> Result<TabsResponse, BrowserError> {
        self.apply(allow, Vec::new())?;
        let mut out = TabsResponse {
            op: req.op,
            tabs: None,
            tab_id: None,
        };
        match req.op {
            TabsOp::List => {
                let tabs = self.tools()?.tabs();
                let tabs = tabs.map_err(|f| self.failed(f, None))?;
                out.tabs = Some(
                    tabs.into_iter()
                        .take(shape::MAX_TABS)
                        .map(|t| crate::Tab {
                            tab_id: shape::clip(&t.id, 128),
                            url: shape::clip(&t.url, shape::MAX_URL_CHARS),
                            title: shape::clip(&t.title, shape::MAX_LABEL_CHARS),
                        })
                        .collect(),
                );
            }
            TabsOp::Open => {
                let url = req.url.as_deref().unwrap_or("about:blank");
                allow.check(Some(url))?;
                self.latest = None;
                let id = self.tools()?.open_tab(url);
                out.tab_id = Some(shape::clip(&id.map_err(|f| self.failed(f, None))?, 128));
            }
            TabsOp::Close | TabsOp::Switch => {
                let tab_id = req.tab_id.clone().unwrap_or_default();
                self.latest = None;
                let done = if req.op == TabsOp::Close {
                    self.tools()?.close_tab(&tab_id)
                } else {
                    self.tools()?.switch_tab(&tab_id)
                };
                done.map_err(|f| self.failed(f, None))?;
                out.tab_id = Some(tab_id);
            }
        }
        Ok(out)
    }

    /// Asks the browser to quit, for a shutdown, and lets go of the pipe.
    pub fn close(&mut self) {
        if let Some(mut tools) = self.tools.take() {
            tools.close();
        }
        self.reset();
    }

    pub fn wait(
        &mut self,
        req: &WaitRequest,
        timeout: Duration,
    ) -> Result<WaitResponse, BrowserError> {
        let mut args = json!({"seconds": timeout.as_secs_f64()});
        for (key, value) in [
            ("text", &req.text),
            ("url", &req.url),
            ("control", &req.control),
            ("gone", &req.gone),
        ] {
            if let Some(value) = value {
                args[key] = json!(value);
            }
        }
        if let Some(load) = req.load {
            args["load"] = json!(match load {
                crate::LoadState::Domcontentloaded => "domcontentloaded",
                crate::LoadState::Load => "load",
            });
        }
        let started = Instant::now();
        let answer = self.call("browser_wait", args, None)?;
        Ok(WaitResponse {
            seen: answer.get("seen").and_then(Value::as_bool).unwrap_or(false),
            after_ms: answer
                .get("after_ms")
                .and_then(Value::as_u64)
                .unwrap_or_else(|| started.elapsed().as_millis() as u64),
            url: answer
                .get("url")
                .and_then(Value::as_str)
                .map(|u| shape::clip(u, shape::MAX_URL_CHARS)),
        })
    }

    pub fn shot(&mut self) -> Result<ShotResponse, BrowserError> {
        // No `path`: the picture comes back in the answer and is written nowhere.
        let answer = self.call("browser_screenshot", json!({}), None)?;
        let data = answer
            .get("base64")
            .and_then(Value::as_str)
            .ok_or_else(|| BrowserError::Backend("the screenshot has no picture".into()))?;
        let picture = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|err| BrowserError::Backend(format!("the screenshot isn't base64: {err}")))?;
        if picture.len() > MAX_SHOT_BYTES {
            return Err(BrowserError::Backend(format!(
                "the screenshot is {} bytes, more than {MAX_SHOT_BYTES}",
                picture.len()
            )));
        }
        // a PNG over the cap is taken again as a JPEG
        let (format, size) = match shape::png_size(&picture) {
            Some(size) => ("png", Some(size)),
            None => ("jpeg", shape::jpeg_size(&picture)),
        };
        let (width, height) = size.ok_or_else(|| {
            BrowserError::Backend("the screenshot is neither a PNG nor a JPEG".into())
        })?;
        Ok(ShotResponse {
            format,
            bytes: picture.len(),
            data: data.to_string(),
            width,
            height,
        })
    }

    pub fn state(
        &mut self,
        op: StateOp,
        folder: &Path,
        name: &str,
    ) -> Result<StateResponse, BrowserError> {
        let path = folder.join(format!("{name}.json"));
        let mut out = StateResponse {
            op,
            name: name.to_string(),
            cookies: 0,
            origins: Vec::new(),
            pending_origins: Vec::new(),
        };
        match op {
            StateOp::Save => {
                own_folder(folder)?;
                refuse_link(&path)?;
                let answer = self.call("browser_state_save", json!({"path": path}), None)?;
                // ultra-instinct writes it 0600; it's checked, and made so, all the same.
                private_file(&path)?;
                out.cookies = shape::count(&answer, "cookies");
                out.origins = shape::origins(&answer, "origins");
            }
            StateOp::Load => {
                if !folder.exists() || !path.exists() {
                    return Err(BrowserError::Invalid(format!(
                        "this screen has no saved login named {name:?}"
                    )));
                }
                own_folder(folder)?;
                refuse_link(&path)?;
                let meta = std::fs::metadata(&path)
                    .map_err(|err| BrowserError::Backend(err.to_string()))?;
                if !meta.is_file() || meta.mode() & 0o077 != 0 {
                    return Err(BrowserError::Invalid(format!(
                        "the saved login {name:?} isn't a file only box-exec can read"
                    )));
                }
                let answer = self.call("browser_state_load", json!({"path": path}), None)?;
                out.cookies = shape::count(&answer, "cookies");
                out.pending_origins = shape::origins(&answer, "pending_origins");
            }
        }
        Ok(out)
    }

    pub fn run_goal(
        &mut self,
        req: &RunRequest,
        (max_steps, timeout): (u32, Duration),
        allow: &AllowList,
    ) -> Result<RunResponse, BrowserError> {
        let started = Instant::now();
        let confirmed: Vec<String> = req
            .confirm
            .as_deref()
            .and_then(|token| self.confirms.redeem(token, None))
            .into_iter()
            .collect();
        if req.confirm.is_some() && confirmed.is_empty() {
            return Err(BrowserError::Invalid(
                "`confirm` is not a token this screen gave, or it was used or is old".into(),
            ));
        }
        self.apply(allow, confirmed)?;
        self.latest = None;
        let bounds = GoalBounds {
            max_steps,
            deadline: started + timeout,
        };
        let report = self.tools()?.run_goal(&req.goal, &bounds);
        // The confirm was for this run alone.
        self.policy = None;
        let report = report.map_err(|f| self.failed(f, None))?;
        let mut out = RunResponse {
            outcome: Outcome::Done,
            reason: None,
            confirm_label: None,
            confirm: None,
            why: None,
            steps: report
                .steps
                .iter()
                .take(MAX_REPORTED_STEPS)
                .map(|s| Step {
                    action: shape::clip(&s.action, 32),
                    target: shape::clip(&s.target, shape::MAX_LABEL_CHARS),
                    effect: shape::clip(&s.effect, 32),
                })
                .collect(),
            elapsed_ms: 0,
        };
        match report.end {
            GoalEnd::Done => {}
            GoalEnd::Blocked { reason, label } => {
                out.outcome = Outcome::Blocked;
                out.reason = Some(shape::clip(&reason, 400));
                // a person is asked only for a label box-browser's own rules want confirmed: a
                // label off the allow-list gets no token, since a confirm can't let it through
                let confirm_label = label.filter(|l| self.rules.needs_confirm(l));
                if let Some(label) = confirm_label {
                    let label = shape::clip(&label, shape::MAX_LABEL_CHARS);
                    out.confirm = Some(self.confirms.issue(&label, None)?);
                    out.confirm_label = Some(label);
                }
            }
            GoalEnd::Abstained { why } => {
                out.outcome = Outcome::Abstained;
                out.why = Some(shape::clip(&why, 400));
            }
            GoalEnd::Failed { why } => {
                out.outcome = Outcome::Failed;
                out.why = Some(shape::clip(&why, 400));
            }
        }
        out.elapsed_ms = started.elapsed().as_millis() as u64;
        Ok(out)
    }
}

/// `@e5`, `e5` or `5`, alone or after `<snapshot>:`.
pub fn parse_ref(reference: &str) -> Result<(usize, Option<u64>), BrowserError> {
    let invalid = || {
        BrowserError::Invalid(format!(
            "a ref is @eN from POST /v1/browser/snap (or <snapshot>:@eN), not {:?}",
            shape::clip(reference, 40)
        ))
    };
    let digits = |s: &str| !s.is_empty() && s.len() <= 19 && s.bytes().all(|b| b.is_ascii_digit());
    let reference = reference.trim();
    let (snapshot, element) = match reference.split_once(':') {
        Some((id, rest)) if digits(id) => (Some(id.parse().map_err(|_| invalid())?), rest),
        Some(_) => return Err(invalid()),
        None => (None, reference),
    };
    let n = element
        .strip_prefix("@e")
        .or_else(|| element.strip_prefix('e'))
        .unwrap_or(element);
    if !digits(n) {
        return Err(invalid());
    }
    let n: usize = n.parse().map_err(|_| invalid())?;
    if n == 0 {
        return Err(invalid());
    }
    Ok((n, snapshot))
}

/// An address the browser may be sent to: http, https or `about:blank`, with nothing odd in it.
pub fn check_url(url: &str) -> Result<(), BrowserError> {
    let ok_scheme =
        url == "about:blank" || url.starts_with("https://") || url.starts_with("http://");
    if !ok_scheme {
        return Err(BrowserError::Invalid(
            "a page is an http or https URL, or about:blank".into(),
        ));
    }
    if url.len() > MAX_URL_BYTES || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(BrowserError::Invalid(format!(
            "a URL is at most {MAX_URL_BYTES} bytes, with no spaces or control characters"
        )));
    }
    Ok(())
}

/// The folder of `screen`'s saved logins under `root`: `screen` is a Bot id or `main`.
pub fn state_folder(root: &Path, screen: &str) -> Result<PathBuf, BrowserError> {
    let ok = (1..=80).contains(&screen.len())
        && screen
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if !ok {
        return Err(BrowserError::Invalid(format!(
            "no screen folder for {screen:?}"
        )));
    }
    Ok(root.join(screen))
}

/// `folder` exists, is a real folder of this user's, and nobody else may enter it.
fn own_folder(folder: &Path) -> Result<(), BrowserError> {
    let bad = |why: String| BrowserError::Backend(format!("{}: {why}", folder.display()));
    if let Some(parent) = folder.parent() {
        if !parent.is_dir() {
            return Err(bad("the folder for saved logins is missing".into()));
        }
    }
    match std::fs::DirBuilder::new().mode(0o700).create(folder) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(err) => return Err(bad(err.to_string())),
    }
    let meta = std::fs::symlink_metadata(folder).map_err(|e| bad(e.to_string()))?;
    // SAFETY: geteuid has no preconditions.
    let me = unsafe { libc::geteuid() };
    if !meta.is_dir() || meta.uid() != me {
        return Err(bad("isn't a folder of box-exec's".into()));
    }
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(folder, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| bad(e.to_string()))?;
    }
    Ok(())
}

fn refuse_link(path: &Path) -> Result<(), BrowserError> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => Err(BrowserError::Backend(
            format!("{} isn't a plain file", path.display()),
        )),
        _ => Ok(()),
    }
}

/// The saved file is a plain file, mode 0600.
fn private_file(path: &Path) -> Result<(), BrowserError> {
    refuse_link(path)?;
    let meta = std::fs::symlink_metadata(path)
        .map_err(|err| BrowserError::Backend(format!("the login wasn't saved: {err}")))?;
    if meta.mode() & 0o777 != 0o600 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|err| BrowserError::Backend(err.to_string()))?;
    }
    Ok(())
}
