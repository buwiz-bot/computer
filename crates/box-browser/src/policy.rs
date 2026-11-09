//! box-browser's own rules, checked before ultra-instinct's guard checks them again.
//!
//! - **Confirm before a press** ([`ENV_CONFIRM_PRESS`], on by default with [`DEFAULT_CONFIRM_PRESS`]):
//!   a click on an element whose label matches is refused with `confirm_required`, the label and a
//!   one-time token. A person approves the label, and the caller sends the token back as
//!   `confirm`. The token is good once, for that element on that screen, for [`CONFIRM_TTL`].
//! - **Allowed domains**, per call (`allow_domains`): the broker sends the Bot's list with every
//!   call, so a screen never keeps a list that was some other Bot's. Off the list, a page gets no
//!   input and isn't navigated to. No list, or an empty one, is no restriction.
//!
//! Globs and hosts mean what ultra-instinct's guard says they mean: its `LabelGlob` and
//! `HostPattern` do the matching.

use std::io::Read;
use std::time::{Duration, Instant};

use ultra_instinct_guard::{host_of, HostPattern, LabelGlob};

use crate::BrowserError;

/// Comma list of label globs whose click needs a person's confirm.
pub const ENV_CONFIRM_PRESS: &str = "BOX_BROWSER_CONFIRM_PRESS";
/// What [`ENV_CONFIRM_PRESS`] is when it isn't set. Set it empty to confirm nothing.
pub const DEFAULT_CONFIRM_PRESS: &str = "Pay*,Delete*,Buy*,Place order*";
/// How long a confirm token is good for.
pub const CONFIRM_TTL: Duration = Duration::from_secs(600);
/// Confirm tokens kept per screen; the oldest goes first.
const MAX_PENDING: usize = 16;
/// Hosts in one allow-list.
pub const MAX_ALLOW_DOMAINS: usize = 64;

/// The confirm globs, for every screen.
#[derive(Clone, Debug)]
pub struct Rules {
    confirm_press: Vec<LabelGlob>,
    raw: Vec<String>,
}

impl Rules {
    /// From [`ENV_CONFIRM_PRESS`]. A list that doesn't parse is reported and the defaults are
    /// used: the rails stay on.
    pub fn from_env() -> Self {
        let raw = std::env::var(ENV_CONFIRM_PRESS).unwrap_or_else(|_| DEFAULT_CONFIRM_PRESS.into());
        Self::parse(&raw).unwrap_or_else(|err| {
            eprintln!("{ENV_CONFIRM_PRESS}: {err}; using {DEFAULT_CONFIRM_PRESS:?}");
            Self::parse(DEFAULT_CONFIRM_PRESS).expect("the default globs parse")
        })
    }

    pub fn parse(list: &str) -> Result<Self, String> {
        let raw: Vec<String> = list
            .split(',')
            .map(str::trim)
            .filter(|g| !g.is_empty())
            .map(str::to_string)
            .collect();
        let confirm_press = raw
            .iter()
            .map(|g| LabelGlob::parse(g).map_err(|err| err.to_string()))
            .collect::<Result<_, _>>()?;
        Ok(Self { confirm_press, raw })
    }

    /// Whether a click on `label` needs a confirm.
    pub fn needs_confirm(&self, label: &str) -> bool {
        self.confirm_press.iter().any(|g| g.matches(label))
    }

    pub fn globs(&self) -> &[String] {
        &self.raw
    }
}

impl Default for Rules {
    fn default() -> Self {
        Self::parse(DEFAULT_CONFIRM_PRESS).expect("the default globs parse")
    }
}

/// One call's allow-list.
#[derive(Clone, Debug, Default)]
pub struct AllowList {
    hosts: Vec<HostPattern>,
    raw: Vec<String>,
}

impl AllowList {
    pub fn parse(list: Option<&[String]>) -> Result<Self, BrowserError> {
        let list = list.unwrap_or_default();
        if list.len() > MAX_ALLOW_DOMAINS {
            return Err(BrowserError::Invalid(format!(
                "allow_domains holds at most {MAX_ALLOW_DOMAINS} hosts"
            )));
        }
        let hosts = list
            .iter()
            .map(|h| {
                HostPattern::parse(h)
                    .map_err(|err| BrowserError::Invalid(format!("allow_domains: {err}")))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            hosts,
            raw: list.to_vec(),
        })
    }

    pub fn raw(&self) -> &[String] {
        &self.raw
    }

    /// May the browser go to `url`, or give input on a page there? A URL with no host
    /// (`about:blank`) may; an unknown one may not while there's a list.
    pub fn check(&self, url: Option<&str>) -> Result<(), BrowserError> {
        if self.hosts.is_empty() {
            return Ok(());
        }
        let Some(url) = url else {
            return Err(BrowserError::DomainNotAllowed(
                "the page's address is unknown".into(),
            ));
        };
        match host_of(url) {
            None => Ok(()),
            Some(host) if self.hosts.iter().any(|p| p.matches(&host)) => Ok(()),
            Some(host) => Err(BrowserError::DomainNotAllowed(host)),
        }
    }
}

/// A click waiting for a person's confirm.
#[derive(Clone, Debug)]
struct Pending {
    token: String,
    label: String,
    /// The element, or `None` for a goal run.
    target: Option<String>,
    expires: Instant,
}

/// One screen's confirm tokens.
#[derive(Debug, Default)]
pub struct Confirms {
    pending: Vec<Pending>,
}

impl Confirms {
    /// A token for a click on `target` labelled `label`.
    pub fn issue(&mut self, label: &str, target: Option<&str>) -> Result<String, BrowserError> {
        let now = Instant::now();
        self.pending.retain(|p| p.expires > now);
        if self.pending.len() >= MAX_PENDING {
            self.pending.remove(0);
        }
        let token = new_token()?;
        self.pending.push(Pending {
            token: token.clone(),
            label: label.to_string(),
            target: target.map(str::to_string),
            expires: now + CONFIRM_TTL,
        });
        Ok(token)
    }

    /// The label `token` confirms for `target`, leaving it unspent: a call checks the token first
    /// and [`Self::spend`]s it only once the press goes out, so a call that fails before then
    /// keeps the person's approval. A token for another target, an old one or a spent one
    /// confirms nothing.
    pub fn peek(&mut self, token: &str, target: Option<&str>) -> Option<String> {
        let now = Instant::now();
        self.pending.retain(|p| p.expires > now);
        self.pending
            .iter()
            .find(|p| p.token == token.trim() && p.target.as_deref() == target)
            .map(|p| p.label.clone())
    }

    /// `token` is spent: it confirms nothing again.
    pub fn spend(&mut self, token: &str) {
        self.pending.retain(|p| p.token != token.trim());
    }

    /// [`Self::peek`], then [`Self::spend`].
    pub fn redeem(&mut self, token: &str, target: Option<&str>) -> Option<String> {
        let label = self.peek(token, target)?;
        self.spend(token);
        Some(label)
    }
}

/// A token nobody can guess, or none at all: no randomness is no confirm.
fn new_token() -> Result<String, BrowserError> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|err| {
            BrowserError::Backend(format!("no randomness for a confirm token: {err}"))
        })?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_globs_catch_pay_delete_buy_and_place_order() {
        let rules = Rules::default();
        for label in ["Pay $18", "Delete account", "Buy now", "Place order"] {
            assert!(rules.needs_confirm(label), "{label}");
        }
        assert!(!rules.needs_confirm("Search"));
        assert!(Rules::parse("").unwrap().globs().is_empty());
    }

    #[test]
    fn an_allow_list_lets_its_hosts_and_hostless_pages_through() {
        let list =
            AllowList::parse(Some(&["shop.example".into(), "*.bank.example".into()])).unwrap();
        assert!(list.check(Some("https://shop.example/cart")).is_ok());
        assert!(list.check(Some("https://www.bank.example/")).is_ok());
        assert!(list.check(Some("about:blank")).is_ok());
        assert!(matches!(
            list.check(Some("https://evil.example/")),
            Err(BrowserError::DomainNotAllowed(host)) if host == "evil.example"
        ));
        assert!(list.check(None).is_err());
        assert!(AllowList::parse(None).unwrap().check(None).is_ok());
        assert!(AllowList::parse(Some(&["not a host".into()])).is_err());
    }

    #[test]
    fn a_token_is_good_once_for_its_target() {
        let mut confirms = Confirms::default();
        let token = confirms.issue("Pay $18", Some("n100")).unwrap();
        assert_eq!(token.len(), 32);
        assert_eq!(confirms.redeem(&token, Some("n200")), None);
        assert_eq!(
            confirms.redeem(&token, Some("n100")).as_deref(),
            Some("Pay $18")
        );
        assert_eq!(confirms.redeem(&token, Some("n100")), None);
        assert_eq!(confirms.redeem("guess", Some("n100")), None);
    }
}
