//! Where ultra-instinct plugs in: the only part of box-browser that knows its types.
//!
//! Everything else speaks [`Tools`]: call one of ultra-instinct's `browser_*` tools by name with
//! JSON arguments, get JSON back or a [`ToolFailure`]; set the policy its guard enforces; run a
//! goal with Instinct. A screen's [`Tools`] is made, used and dropped on that screen's own thread
//! (`crate::screen`), so it need not be `Send`: ultra-instinct's pipe and server are not.
//!
//! [`open`] is the seam. It takes the screen's end of Chromium's DevTools pipe, a unix socket
//! `box-exec` was handed (`box_chrome::pipe`), and gives back the screen's tools: an
//! `ultra_instinct_cdp::PipeBrowser` on it, and an `ultra_instinct_mcp::Server` whose connector
//! attaches to a tab of that browser.

use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::time::{Duration, Instant};

use serde_json::Value;
use ultra_instinct_cdp::{CdpError, CdpTransport, PipeBrowser, PipeLimits};
use ultra_instinct_guard::Policy;
use ultra_instinct_mcp::{Server, ToolError};

/// Whether [`open`] can speak over a pipe in this build. While it can't, box-browser never takes
/// a pipe: one taken and dropped would close the screen's Chromium.
pub const LINKED: bool = true;

/// The largest frame the pipe takes: ultra-instinct's own default. A frame over it closes the
/// screen's Chromium for good, so it sits well above box-browser's screenshot cap: a full-page
/// shot comes back base64, a third larger than the PNG, and an answer the shot cap refuses must
/// still arrive whole to be refused.
pub const MAX_FRAME_BYTES: usize = 64 << 20;
/// How long one CDP call on the pipe waits for its answer.
pub const CALL_DEADLINE: Duration = Duration::from_secs(20);

/// The `cdp` key a tool call names a tab by. Every tool call goes to the current tab this way, so
/// ultra-instinct's server keeps one session per tab, each attached over the one pipe.
const KEY_PREFIX: &str = "pipe:";

/// The screen's tools on `pipe`, its end of Chromium's DevTools pipe.
///
/// `box-chrome-pipe` gives Chromium one end of a socket pair as both fd 3 (Chromium reads) and
/// fd 4 (Chromium writes), so `pipe`, the other end, is both directions: what Chromium writes on
/// fd 4 is read here, and what's written here Chromium reads on fd 3. ultra-instinct's
/// `PipeBrowser::new(reader, writer)` takes Chromium's fd 4 side as the reader and its fd 3 side
/// as the writer, so both are this socket. The socket counts as closed for Chromium only once
/// every copy of this end is: both live in the `PipeBrowser`, and go with it.
pub fn open(pipe: OwnedFd) -> Result<Box<dyn Tools>, ToolFailure> {
    let reader = pipe
        .try_clone()
        .map_err(|err| ToolFailure::Unavailable(format!("the pipe can't be shared: {err}")))?;
    let limits = PipeLimits {
        max_frame_bytes: MAX_FRAME_BYTES,
        call_deadline: CALL_DEADLINE,
    };
    let browser = Rc::new(PipeBrowser::with_limits(reader, pipe, limits));
    // Tabs come and go: their `Target.targetDestroyed` is how a page learns its tab is gone.
    browser
        .call("Target.setDiscoverTargets", r#"{"discover":true}"#)
        .map_err(failure_of)?;
    let tabs = browser.clone();
    let server = Server::with_connector(move |key: &str| {
        let target = key.strip_prefix(KEY_PREFIX).unwrap_or(key);
        tabs.page(target)
            .map(|page| Box::new(page) as Box<dyn CdpTransport>)
    });
    Ok(Box::new(ServerTools::on_pipe(server, browser)))
}

/// A pipe error as a tool failure.
pub fn failure_of(err: CdpError) -> ToolFailure {
    let message = err.to_string();
    match err {
        CdpError::PipeClosed { .. } | CdpError::FrameTooLarge { .. } => {
            ToolFailure::Closed(message)
        }
        CdpError::TargetGone { .. } => ToolFailure::TargetGone(message),
        CdpError::DialogOpened { .. } => ToolFailure::DialogOpen(message),
        CdpError::Transport { .. }
            if message.contains("timed out") || message.contains("deadline") =>
        {
            ToolFailure::Timeout(message)
        }
        _ => ToolFailure::Browser(message),
    }
}

/// A tab, as box-browser lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabInfo {
    pub id: String,
    pub url: String,
    pub title: String,
}

/// What box-browser asks of ultra-instinct, for one screen.
pub trait Tools {
    /// One `browser_*` tool. box-browser calls a fixed few (`crate::screen`), and never the ones
    /// that read or set cookies, storage, headers or the network, or that download.
    fn call(&mut self, name: &str, args: &Value) -> Result<Value, ToolFailure>;
    /// The policy ultra-instinct's own guard enforces from now on, beside box-browser's checks.
    fn set_policy(&mut self, policy: &PolicyLists) -> Result<(), ToolFailure>;
    /// Instinct's loop on the current tab, under the policy last set, until it's done, blocked or
    /// abstains, or a bound runs out.
    fn run_goal(&mut self, goal: &str, bounds: &GoalBounds) -> Result<GoalReport, ToolFailure>;
    /// The browser's page tabs.
    fn tabs(&mut self) -> Result<Vec<TabInfo>, ToolFailure>;
    /// A new tab on `url`, which every later call drives; its id.
    fn open_tab(&mut self, url: &str) -> Result<String, ToolFailure>;
    /// Brings tab `id` to the front, and every later call drives it.
    fn switch_tab(&mut self, id: &str) -> Result<(), ToolFailure>;
    fn close_tab(&mut self, id: &str) -> Result<(), ToolFailure>;
    /// Asks the browser to quit (`Browser.close`), for a shutdown. Best effort.
    fn close(&mut self) {}
}

/// A policy, as the lists it's made from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PolicyLists {
    /// `shop.example`, `*.shop.example`. Empty: any host.
    pub allow_domains: Vec<String>,
    /// Label globs whose click needs a confirm.
    pub confirm_press: Vec<String>,
    /// Labels a person confirmed, for the next call alone.
    pub confirmed: Vec<String>,
}

impl PolicyLists {
    pub fn to_policy(&self) -> Result<Policy, ToolFailure> {
        let mut policy =
            Policy::from_lists(&self.allow_domains.join(","), &self.confirm_press.join(","))
                .map_err(|err| ToolFailure::Invalid(err.to_string()))?;
        for label in &self.confirmed {
            policy = policy.with_confirmed(label.clone());
        }
        Ok(policy)
    }
}

/// A goal run's bounds.
#[derive(Clone, Copy, Debug)]
pub struct GoalBounds {
    pub max_steps: u32,
    pub deadline: Instant,
}

/// How a goal run ended, in box-browser's words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GoalEnd {
    Done,
    /// The guard or the policy stopped it. `confirm_label` is the label a person may confirm.
    Blocked {
        reason: String,
        confirm_label: Option<String>,
    },
    /// Instinct wouldn't choose: the caller's model may.
    Abstained {
        why: String,
    },
    Failed {
        why: String,
    },
}

/// One verified step of a goal run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoalStep {
    /// `click`, `type`, `select`, `key`, …
    pub action: String,
    /// The target's label.
    pub target: String,
    /// What the step did: `success`, `no-effect`, `navigation`, …
    pub effect: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoalReport {
    pub end: GoalEnd,
    pub steps: Vec<GoalStep>,
}

/// Why a tool call failed. ultra-instinct's own errors are mapped onto these once, here.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ToolFailure {
    /// The arguments were refused before anything was done.
    #[error("{0}")]
    Invalid(String),
    /// The press needs a person's confirm of this label.
    #[error("needs a confirm: {label}")]
    NeedsConfirm { label: String },
    /// The page's host isn't on the allow-list.
    #[error("not an allowed domain: {0}")]
    DomainNotAllowed(String),
    /// The guard refused: the page drifted, the target is covered or disabled, the ticket is spent.
    #[error("{0}")]
    Refused(String),
    /// The pipe or the browser is gone: the next call opens the screen again.
    #[error("{0}")]
    Closed(String),
    /// The element a ref named isn't on the page, or the ref isn't one ultra-instinct knows.
    #[error("{0}")]
    Stale(String),
    /// The tab is gone.
    #[error("{0}")]
    TargetGone(String),
    /// A JavaScript dialog is open on the tab, and nothing else gets through until it's answered.
    #[error("{0}")]
    DialogOpen(String),
    /// The browser didn't answer in time.
    #[error("{0}")]
    Timeout(String),
    /// box-browser called a tool ultra-instinct's allow-list doesn't let it: a bug here.
    #[error("{0}")]
    NotAllowed(String),
    /// This can't be done here at all.
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    Browser(String),
}

impl From<ToolError> for ToolFailure {
    fn from(err: ToolError) -> Self {
        let message = err.to_string();
        // ultra-instinct's stable codes, where it gives one; its variant names before that.
        let wire = err.to_value();
        if let Some(failure) = wire
            .get("code")
            .and_then(Value::as_str)
            .and_then(|code| from_code(code, &message, &wire))
        {
            return failure;
        }
        // Until those codes come, a pipe error reaches here as its text.
        if message.contains("CDP pipe closed") || message.contains("pipe frame exceeds") {
            return Self::Closed(message);
        }
        if message.contains("detached or was destroyed") {
            return Self::TargetGone(message);
        }
        if message.contains("a JavaScript dialog opened") {
            return Self::DialogOpen(message);
        }
        match err.variant() {
            "TicketInvalid" => refusal(&message),
            "InvalidArguments" | "UnknownTool" | "EmptyText" | "UnknownRole" | "UnknownAction"
            | "UnsupportedAction" | "MissingToolName" => Self::Invalid(message),
            _ => Self::Browser(message),
        }
    }
}

/// One of ultra-instinct's stable error codes, or `None` for one this doesn't know.
fn from_code(code: &str, message: &str, wire: &Value) -> Option<ToolFailure> {
    let message = message.to_string();
    Some(match code {
        "needs_confirm" => match wire.get("label").and_then(Value::as_str) {
            Some(label) => ToolFailure::NeedsConfirm {
                label: label.to_string(),
            },
            // A confirm with no label to show a person is no confirm: refuse.
            None => ToolFailure::Refused(message),
        },
        "domain_not_allowed" => ToolFailure::DomainNotAllowed(
            wire.get("host")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ),
        "stale_ref" | "unknown_ref" => ToolFailure::Stale(message),
        "target_gone" => ToolFailure::TargetGone(message),
        "dialog_open" => ToolFailure::DialogOpen(message),
        "timeout" => ToolFailure::Timeout(message),
        "pipe_closed" => ToolFailure::Closed(message),
        "tool_not_allowed" => ToolFailure::NotAllowed(message),
        "invalid_arguments" => ToolFailure::Invalid(message),
        _ => return None,
    })
}

/// ultra-instinct's `TicketInvalid` text: `needs-confirm: "<label as JSON>"`,
/// `domain-not-allowed: <host>`, or another refusal.
fn refusal(message: &str) -> ToolFailure {
    let body = message
        .split_once("needs-confirm: ")
        .map(|(_, rest)| rest.trim());
    if let Some(raw) = body {
        let label = serde_json::from_str::<String>(raw).unwrap_or_else(|_| raw.to_string());
        return ToolFailure::NeedsConfirm { label };
    }
    if let Some((_, host)) = message.split_once("domain-not-allowed: ") {
        return ToolFailure::DomainNotAllowed(host.trim().to_string());
    }
    if message.contains("domain-not-allowed") {
        return ToolFailure::DomainNotAllowed(String::new());
    }
    ToolFailure::Refused(message.to_string())
}

/// [`Tools`] on an `ultra_instinct_mcp::Server`, and on the pipe its tabs are attached over.
pub struct ServerTools {
    server: Option<Server>,
    pipe: Option<Rc<PipeBrowser>>,
    /// The tab every tool call drives, once one is chosen.
    current: Option<String>,
}

impl ServerTools {
    /// Tools on a server that finds its own pages (a replay, in tests): no tabs.
    pub fn new(server: Server) -> Self {
        Self {
            server: Some(server),
            pipe: None,
            current: None,
        }
    }

    fn on_pipe(server: Server, pipe: Rc<PipeBrowser>) -> Self {
        Self {
            server: Some(server),
            pipe: Some(pipe),
            current: None,
        }
    }

    fn server(&mut self) -> Result<&mut Server, ToolFailure> {
        self.server
            .as_mut()
            .ok_or_else(|| ToolFailure::Closed("the browser's server is gone".into()))
    }

    fn pipe(&self) -> Result<Rc<PipeBrowser>, ToolFailure> {
        self.pipe
            .clone()
            .ok_or_else(|| ToolFailure::Unavailable("this browser has no tabs to list".into()))
    }

    /// The tab tool calls go to: the one chosen, else the browser's first page tab, else a new
    /// blank one.
    fn current(&mut self) -> Result<String, ToolFailure> {
        if let Some(id) = &self.current {
            return Ok(id.clone());
        }
        let pipe = self.pipe()?;
        let first = pipe.targets().map_err(failure_of)?.into_iter().next();
        let id = match first {
            Some(tab) => tab.target_id,
            None => pipe.create_target("about:blank").map_err(failure_of)?,
        };
        self.current = Some(id.clone());
        Ok(id)
    }
}

impl Tools for ServerTools {
    fn call(&mut self, name: &str, args: &Value) -> Result<Value, ToolFailure> {
        let mut args = args.clone();
        if self.pipe.is_some() {
            args["cdp"] = Value::String(format!("{KEY_PREFIX}{}", self.current()?));
        }
        let answer = self
            .server()?
            .call_tool(name, &args)
            .map_err(ToolFailure::from);
        if matches!(answer, Err(ToolFailure::TargetGone(_))) {
            // The next call picks a tab that is there.
            self.current = None;
        }
        answer
    }

    fn set_policy(&mut self, policy: &PolicyLists) -> Result<(), ToolFailure> {
        // No runtime setter yet: the same server, its sessions and ledgers, under a new policy.
        let policy = policy.to_policy()?;
        let server = self
            .server
            .take()
            .ok_or_else(|| ToolFailure::Closed("the browser's server is gone".into()))?;
        self.server = Some(server.with_policy(policy));
        Ok(())
    }

    fn run_goal(&mut self, _goal: &str, _bounds: &GoalBounds) -> Result<GoalReport, ToolFailure> {
        Err(ToolFailure::Unavailable(
            "goal runs aren't in this box-exec yet: they come with ultra-instinct's run_goal"
                .into(),
        ))
    }

    fn tabs(&mut self) -> Result<Vec<TabInfo>, ToolFailure> {
        let tabs = self.pipe()?.targets().map_err(failure_of)?;
        Ok(tabs
            .into_iter()
            .map(|t| TabInfo {
                id: t.target_id,
                url: t.url,
                title: t.title,
            })
            .collect())
    }

    fn open_tab(&mut self, url: &str) -> Result<String, ToolFailure> {
        let id = self.pipe()?.create_target(url).map_err(failure_of)?;
        self.current = Some(id.clone());
        Ok(id)
    }

    fn switch_tab(&mut self, id: &str) -> Result<(), ToolFailure> {
        let pipe = self.pipe()?;
        if !pipe
            .targets()
            .map_err(failure_of)?
            .iter()
            .any(|t| t.target_id == id)
        {
            return Err(ToolFailure::TargetGone(format!("there is no tab {id}")));
        }
        pipe.activate_target(id).map_err(failure_of)?;
        self.current = Some(id.to_string());
        Ok(())
    }

    fn close_tab(&mut self, id: &str) -> Result<(), ToolFailure> {
        let pipe = self.pipe()?;
        if !pipe
            .targets()
            .map_err(failure_of)?
            .iter()
            .any(|t| t.target_id == id)
        {
            return Err(ToolFailure::TargetGone(format!("there is no tab {id}")));
        }
        if !pipe.close_target(id).map_err(failure_of)? {
            return Err(ToolFailure::Browser(format!(
                "the browser kept tab {id} open"
            )));
        }
        if self.current.as_deref() == Some(id) {
            self.current = None;
        }
        Ok(())
    }

    fn close(&mut self) {
        // The server's sessions go first, so their detaches don't race the browser going away.
        self.server = None;
        if let Some(pipe) = self.pipe.take() {
            let _ = pipe.call("Browser.close", "{}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ultra_instinct_refusals_are_named() {
        assert_eq!(
            refusal(r#"ticket invalid: needs-confirm: "Pay $18""#),
            ToolFailure::NeedsConfirm {
                label: "Pay $18".into()
            }
        );
        assert_eq!(
            refusal("domain-not-allowed: evil.example"),
            ToolFailure::DomainNotAllowed("evil.example".into())
        );
        assert!(matches!(refusal("target-covered"), ToolFailure::Refused(_)));
    }

    #[test]
    fn a_tool_error_maps_by_its_variant() {
        let err = ToolError::InvalidArguments("no".into());
        assert!(matches!(ToolFailure::from(err), ToolFailure::Invalid(_)));
        let err = ToolError::Browser("socket".into());
        assert!(matches!(ToolFailure::from(err), ToolFailure::Browser(_)));
        let err = ToolError::TicketInvalid(r#"needs-confirm: "Delete""#.into());
        assert_eq!(
            ToolFailure::from(err),
            ToolFailure::NeedsConfirm {
                label: "Delete".into()
            }
        );
    }

    #[test]
    fn ultra_instinct_s_codes_map_one_to_one() {
        let wire = |code: &str| serde_json::json!({"code": code, "message": "m"});
        let cases = [
            ("stale_ref", "Stale"),
            ("unknown_ref", "Stale"),
            ("target_gone", "TargetGone"),
            ("dialog_open", "DialogOpen"),
            ("timeout", "Timeout"),
            ("pipe_closed", "Closed"),
            ("tool_not_allowed", "NotAllowed"),
            ("invalid_arguments", "Invalid"),
        ];
        for (code, want) in cases {
            let got = from_code(code, "m", &wire(code)).unwrap();
            assert!(format!("{got:?}").starts_with(want), "{code} -> {got:?}");
        }
        let confirm = serde_json::json!({"code": "needs_confirm", "label": "Pay $18"});
        assert_eq!(
            from_code("needs_confirm", "m", &confirm),
            Some(ToolFailure::NeedsConfirm {
                label: "Pay $18".into()
            })
        );
        // No label is no confirm.
        assert!(matches!(
            from_code("needs_confirm", "m", &wire("needs_confirm")),
            Some(ToolFailure::Refused(_))
        ));
        assert!(from_code("something_new", "m", &wire("x")).is_none());
    }

    #[test]
    fn a_bad_list_is_invalid() {
        let lists = PolicyLists {
            allow_domains: vec!["not a host".into()],
            ..Default::default()
        };
        assert!(matches!(lists.to_policy(), Err(ToolFailure::Invalid(_))));
    }

    #[test]
    fn a_pipe_whose_browser_is_gone_is_closed() {
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        drop(b);
        assert!(matches!(open(a.into()), Err(ToolFailure::Closed(_))));
    }

    #[test]
    fn pipe_errors_keep_their_kind() {
        let closed = CdpError::PipeClosed {
            message: "eof".into(),
        };
        assert!(matches!(failure_of(closed), ToolFailure::Closed(_)));
        let gone = CdpError::TargetGone {
            target_id: "T".into(),
        };
        assert!(matches!(failure_of(gone), ToolFailure::TargetGone(_)));
        let dialog = CdpError::DialogOpened {
            method: "Input.dispatchMouseEvent".into(),
        };
        assert!(matches!(failure_of(dialog), ToolFailure::DialogOpen(_)));
        // And the same errors as a tool's text.
        let tool = ToolError::Browser("CDP pipe closed: eof".into());
        assert!(matches!(ToolFailure::from(tool), ToolFailure::Closed(_)));
        let tool = ToolError::Browser("CDP target T detached or was destroyed".into());
        assert!(matches!(
            ToolFailure::from(tool),
            ToolFailure::TargetGone(_)
        ));
    }
}
