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
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use ultra_instinct_cdp::{PipeBrowser, PipeLimits};
use ultra_instinct_guard::Policy;
use ultra_instinct_mcp::{
    BlockKind, Downloads, RunLimits, RunOutcome, Server, ToolError, ToolLimits, Uploads,
};

/// The largest frame the pipe takes. A frame over it costs only the call it answers
/// (`frame_too_large`): the pipe and the screen's other tabs go on. It sits well above the
/// screenshot cap, since a shot comes back base64, a third larger than the picture.
pub const MAX_FRAME_BYTES: usize = 64 << 20;
/// How long one CDP call on the pipe waits for its answer, its write included.
pub const CALL_DEADLINE: Duration = Duration::from_secs(20);
/// The largest picture ultra-instinct returns: past it, it takes the shot again as a JPEG, and
/// refuses one still over. box-browser's own cap ([`crate::screen::MAX_SHOT_BYTES`]).
pub const MAX_SHOT_BYTES: usize = crate::screen::MAX_SHOT_BYTES;

/// The only tools a screen's server answers: reading and pressing, the tabs, saving and loading
/// a login, and a goal run. Never a script (`browser_exec`), the DOM, cookies, storage, headers,
/// the network, a download, an upload or a PDF; and none of the keyboard-only tools
/// (`browser_keyboard`, `browser_hold_key`): every key box-browser sends names an element, so it
/// has no use for one.
pub const ALLOWED: [&str; 17] = [
    "browser_navigate",
    "browser_get_state",
    "browser_click",
    "browser_type",
    "browser_select_dropdown",
    "browser_send_key",
    "browser_scroll_to",
    "browser_focus",
    "browser_wait",
    "browser_screenshot",
    "browser_state_save",
    "browser_state_load",
    "browser_new_tab",
    "browser_list_tabs",
    "browser_switch_tab",
    "browser_close_tab",
    // `Server::run_goal`: not a tool, but the list gates it by this name
    "run_goal",
];

/// The screen's tools on `pipe`, its end of Chromium's DevTools pipe.
///
/// `box-chrome-pipe` gives Chromium one end of a socket pair as both fd 3 (Chromium reads) and
/// fd 4 (Chromium writes), so `pipe`, the other end, is both directions: what Chromium writes on
/// fd 4 is read here, and what's written here Chromium reads on fd 3. ultra-instinct's
/// `PipeBrowser` takes Chromium's fd 4 side as the reader and its fd 3 side as the writer, so
/// both are this socket. It makes the writer non-blocking, a flag every copy of the socket
/// shares: both copies are its own, and nothing else here keeps one. The socket counts as
/// closed for Chromium only once every copy of this end is: both live in the server, and go
/// with it.
pub fn open(pipe: OwnedFd) -> Result<Box<dyn Tools>, ToolFailure> {
    let reader = pipe
        .try_clone()
        .map_err(|err| ToolFailure::Unavailable(format!("the pipe can't be shared: {err}")))?;
    let limits = PipeLimits {
        max_frame_bytes: MAX_FRAME_BYTES,
        call_deadline: CALL_DEADLINE,
    };
    let browser = PipeBrowser::with_limits(reader, pipe, limits);
    // The tab tools speak the pipe's own `Target.*`; a press needs the snapshot its index was
    // read in, and refuses `stale_ref` when the element it showed has moved or changed.
    let mut server = Server::with_pipe(browser)
        .require_snapshot(true)
        .with_limits(ToolLimits {
            max_screenshot_bytes: MAX_SHOT_BYTES,
            ..ToolLimits::default()
        })
        .with_downloads(Downloads::Off)
        // `browser_file_upload` is off the list too: this is the second lock
        .with_uploads(Uploads::Off)
        .with_tool_allowlist(ALLOWED);
    // A browser already gone says so now, not on the first real call. Any other failure keeps the
    // server: dropping it would close the pipe, and Chromium quits once it closes. Its launcher
    // would have it started again (`crate::pipe::Leased`), but a person at the screen would lose
    // their tabs for what may be a passing failure. The next call finds out whatever this one met.
    if let Err(err) = server.call_tool("browser_list_tabs", &json!({})) {
        let failure = ToolFailure::from(err);
        if matches!(failure, ToolFailure::Closed(_)) {
            return Err(failure);
        }
    }
    Ok(Box::new(ServerTools::new(server)))
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
    /// The policy ultra-instinct's guard holds. Under an allow-list it refuses every `blob:` page,
    /// as box-browser's own list does: so a page a goal run reaches on its own mid-run is refused
    /// by the same rule as one an `act` starts on. With no list it changes nothing.
    pub fn to_policy(&self) -> Result<Policy, ToolFailure> {
        let mut policy =
            Policy::from_lists(&self.allow_domains.join(","), &self.confirm_press.join(","))
                .map_err(|err| ToolFailure::Invalid(err.to_string()))?
                .with_refuse_blob(true);
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
    /// The guard or the policy stopped it, and `by` says which.
    Blocked {
        reason: String,
        by: BlockedBy,
    },
    /// Instinct wouldn't choose: the caller's model may.
    Abstained {
        why: String,
    },
    Failed {
        why: String,
    },
}

/// What stopped a goal run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockedBy {
    /// A press needs a person's confirm of `label`: a control's own, the submit button of the form
    /// a key would send, or the guard's `(form submit)` / `(unobserved focus)` when it couldn't
    /// see which.
    Confirm { label: String },
    /// The page, or the form a press would send, is off the allow-list. `host` is its host, or for
    /// an address with none its scheme (`a blob: page`), as a tool's refusal names it; `None` when
    /// its address couldn't be read. A confirm can't let it through.
    Domain { host: Option<String> },
    /// The run stopped itself.
    Loop,
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
    /// The answer was more than the pipe or the screenshot cap takes. Only this call is lost.
    #[error("{0}")]
    TooLarge(String),
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
        let wire = err.to_value();
        let code = wire
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or("browser");
        from_code(code, message, &wire)
    }
}

/// One of ultra-instinct's stable error codes. A code this doesn't know is the browser's failure.
fn from_code(code: &str, message: String, wire: &Value) -> ToolFailure {
    let field = |key: &str| wire.get(key).and_then(Value::as_str).map(str::to_string);
    match code {
        "needs_confirm" => match field("label") {
            Some(label) => ToolFailure::NeedsConfirm { label },
            // A confirm with no label to show a person is no confirm: refuse.
            None => ToolFailure::Refused(message),
        },
        "domain_not_allowed" => ToolFailure::DomainNotAllowed(field("host").unwrap_or_default()),
        // the element isn't what the caller saw, or the snapshot named is no longer kept
        "stale_ref" | "unknown_ref" | "unknown_snapshot" | "snapshot_evicted" => {
            ToolFailure::Stale(message)
        }
        "target_gone" => ToolFailure::TargetGone(message),
        "dialog_open" => ToolFailure::DialogOpen(message),
        "timeout" => ToolFailure::Timeout(message),
        "pipe_closed" => ToolFailure::Closed(message),
        "frame_too_large" | "output_too_large" => ToolFailure::TooLarge(message),
        // the hard gate refused (disabled, hidden, covered, a spent ticket), or a capture couldn't
        // hide a secret, or no more secrets fit: nothing was done
        "ticket_invalid" | "secret_on_screen" | "secrets_full" => ToolFailure::Refused(message),
        "invalid_arguments" => ToolFailure::Invalid(message),
        // what box-browser sends is wrong: a bug here, not the caller's
        "tool_not_allowed" | "snapshot_required" | "bad_snapshot" | "path_not_absolute"
        | "path_not_allowed" | "downloads_off" | "uploads_off" | "unknown_tool"
        | "missing_tool_name" => ToolFailure::NotAllowed(message),
        _ => ToolFailure::Browser(message),
    }
}

/// [`Tools`] on an `ultra_instinct_mcp::Server` over the pipe.
pub struct ServerTools {
    server: Option<Server>,
}

impl ServerTools {
    pub fn new(server: Server) -> Self {
        Self {
            server: Some(server),
        }
    }

    fn server(&mut self) -> Result<&mut Server, ToolFailure> {
        self.server
            .as_mut()
            .ok_or_else(|| ToolFailure::Closed("the browser's server is gone".into()))
    }

    fn tool(&mut self, name: &str, args: &Value) -> Result<Value, ToolFailure> {
        self.server()?
            .call_tool(name, args)
            .map_err(ToolFailure::from)
    }
}

/// `key`'s text in a tab tool's answer.
fn said(answer: &Value, key: &str) -> Result<String, ToolFailure> {
    answer
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ToolFailure::Browser(format!("the browser's answer has no {key}")))
}

impl Tools for ServerTools {
    fn call(&mut self, name: &str, args: &Value) -> Result<Value, ToolFailure> {
        self.tool(name, args)
    }

    fn set_policy(&mut self, policy: &PolicyLists) -> Result<(), ToolFailure> {
        let policy = policy.to_policy()?;
        self.server()?.set_policy(policy);
        Ok(())
    }

    fn run_goal(&mut self, goal: &str, bounds: &GoalBounds) -> Result<GoalReport, ToolFailure> {
        let limits = RunLimits {
            max_steps: bounds.max_steps,
            deadline: Some(bounds.deadline),
        };
        // `typed` holds what the run typed, secrets too: it goes nowhere.
        let report = self.server()?.run_goal(goal, limits);
        let end = match report.outcome {
            RunOutcome::Done { .. } => GoalEnd::Done,
            RunOutcome::Blocked {
                label,
                reason,
                kind,
            } => GoalEnd::Blocked {
                reason,
                by: match kind {
                    // a confirm with no label to show a person is no confirm
                    Some(BlockKind::NeedsConfirm) if !label.trim().is_empty() => {
                        BlockedBy::Confirm { label }
                    }
                    Some(BlockKind::DomainNotAllowed { host }) => BlockedBy::Domain { host },
                    _ => BlockedBy::Loop,
                },
            },
            RunOutcome::Abstained { why } => GoalEnd::Abstained { why },
            // refused before the loop began: the same failure a tool call would be, so a pipe
            // that went resets the screen
            RunOutcome::Failed {
                error,
                code: Some(code),
            } => return Err(from_code(code, error, &Value::Null)),
            // the loop's own: the browser failed, or the steps or the time ran out
            RunOutcome::Failed { error, code: None } => GoalEnd::Failed { why: error },
        };
        Ok(GoalReport {
            end,
            steps: report
                .steps
                .into_iter()
                .map(|s| GoalStep {
                    action: s.action.to_ascii_lowercase(),
                    target: s.target_label,
                    effect: s.effect,
                })
                .collect(),
        })
    }

    fn tabs(&mut self) -> Result<Vec<TabInfo>, ToolFailure> {
        let answer = self.tool("browser_list_tabs", &json!({}))?;
        let tabs = answer
            .get("tabs")
            .and_then(Value::as_array)
            .ok_or_else(|| ToolFailure::Browser("the browser's answer has no tabs".into()))?;
        Ok(tabs
            .iter()
            .filter_map(|t| {
                Some(TabInfo {
                    id: t.get("tab_id")?.as_str()?.to_string(),
                    url: t
                        .get("url")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    title: t
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
            })
            .collect())
    }

    fn open_tab(&mut self, url: &str) -> Result<String, ToolFailure> {
        let answer = self.tool("browser_new_tab", &json!({"url": url}))?;
        said(&answer, "tab_id")
    }

    fn switch_tab(&mut self, id: &str) -> Result<(), ToolFailure> {
        self.tool("browser_switch_tab", &json!({"tab_id": id}))
            .map(drop)
    }

    fn close_tab(&mut self, id: &str) -> Result<(), ToolFailure> {
        let answer = self.tool("browser_close_tab", &json!({"tab_id": id}))?;
        if answer.get("closed").and_then(Value::as_bool) == Some(false) {
            return Err(ToolFailure::Browser(format!(
                "the browser kept tab {id} open"
            )));
        }
        Ok(())
    }

    fn close(&mut self) {
        // Every handle on the pipe goes with the server: Chromium quits a second later.
        self.server = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ultra_instinct_s_codes_map_one_to_one() {
        let wire = |code: &str| json!({"code": code, "message": "m"});
        let cases = [
            ("stale_ref", "Stale"),
            ("unknown_ref", "Stale"),
            ("unknown_snapshot", "Stale"),
            ("snapshot_evicted", "Stale"),
            ("target_gone", "TargetGone"),
            ("dialog_open", "DialogOpen"),
            ("timeout", "Timeout"),
            ("pipe_closed", "Closed"),
            ("frame_too_large", "TooLarge"),
            ("output_too_large", "TooLarge"),
            ("ticket_invalid", "Refused"),
            ("secret_on_screen", "Refused"),
            ("secrets_full", "Refused"),
            ("invalid_arguments", "Invalid"),
            ("tool_not_allowed", "NotAllowed"),
            ("snapshot_required", "NotAllowed"),
            ("path_not_absolute", "NotAllowed"),
            ("downloads_off", "NotAllowed"),
            ("uploads_off", "NotAllowed"),
            ("browser", "Browser"),
            ("something_new", "Browser"),
        ];
        for (code, want) in cases {
            let got = from_code(code, "m".into(), &wire(code));
            assert!(format!("{got:?}").starts_with(want), "{code} -> {got:?}");
        }
        let confirm = json!({"code": "needs_confirm", "label": "Pay $18"});
        assert_eq!(
            from_code("needs_confirm", "m".into(), &confirm),
            ToolFailure::NeedsConfirm {
                label: "Pay $18".into()
            }
        );
        // No label is no confirm.
        assert!(matches!(
            from_code("needs_confirm", "m".into(), &wire("needs_confirm")),
            ToolFailure::Refused(_)
        ));
        let host = json!({"code": "domain_not_allowed", "host": "evil.example"});
        assert_eq!(
            from_code("domain_not_allowed", "m".into(), &host),
            ToolFailure::DomainNotAllowed("evil.example".into())
        );
    }

    /// ultra-instinct's own allow-list, by default, lets a `blob:` page through by its origin; the
    /// policy box-browser gives it refuses every one under a list, as box-browser's own list
    /// (`policy::AllowList::check`) does. Should upstream's default or its switch change, this
    /// says so at the next bump.
    #[test]
    fn the_policy_box_browser_builds_refuses_a_blob_as_its_own_list_does() {
        let lists = PolicyLists {
            allow_domains: vec!["shop.example".into()],
            ..PolicyLists::default()
        };
        let blob = "blob:https://shop.example/0b7c6a3e";
        let upstream_default =
            Policy::from_lists(&lists.allow_domains.join(","), "").expect("the list parses");
        assert!(upstream_default.check_url(blob).is_ok());
        let built = lists.to_policy().unwrap();
        assert!(built.refuse_blob);
        assert!(built.check_url(blob).is_err());
        let ours = crate::policy::AllowList::parse(Some(lists.allow_domains.as_slice())).unwrap();
        assert!(ours.check(Some(blob)).is_err());
        // what all three refuse, and what all three let through
        for url in ["data:text/html,hi", "javascript:1", "file:///etc/passwd"] {
            assert!(upstream_default.check_url(url).is_err(), "{url}");
            assert!(built.check_url(url).is_err(), "{url}");
            assert!(ours.check(Some(url)).is_err(), "{url}");
        }
        for url in ["https://shop.example/cart", "about:blank"] {
            assert!(built.check_url(url).is_ok(), "{url}");
            assert!(ours.check(Some(url)).is_ok(), "{url}");
        }
        // with no list, the switch lets everything through, as box-browser's empty list does
        let open = PolicyLists::default().to_policy().unwrap();
        assert!(open.check_url(blob).is_ok());
    }

    #[test]
    fn a_tool_error_maps_by_its_code() {
        let err = ToolError::InvalidArguments("no".into());
        assert!(matches!(ToolFailure::from(err), ToolFailure::Invalid(_)));
        let err = ToolError::Browser("socket".into());
        assert!(matches!(ToolFailure::from(err), ToolFailure::Browser(_)));
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
    fn nothing_that_reads_a_field_or_touches_files_is_allowed() {
        for never in [
            "browser_exec",
            "browser_javascript_exec",
            "browser_get_html",
            "browser_cookies",
            "browser_storage",
            "browser_set",
            "browser_network",
            "browser_download",
            "browser_file_upload",
            "browser_save_as_pdf",
            "browser_keyboard",
            "browser_hold_key",
        ] {
            assert!(!ALLOWED.contains(&never), "{never}");
        }
    }
}
