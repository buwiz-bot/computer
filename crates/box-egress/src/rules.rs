//! A computer's browsing rules: where its browser's traffic leaves from, and which sites it may
//! reach. One set per computer, because a box has one proxy and one tunnel: every screen's
//! browser on it, every Bot's, goes through them.
//!
//! The rules are judged on the name the browser asked the proxy for (the CONNECT host), before
//! any DNS. Names, not addresses: the address a name resolves to is judged separately, after DNS,
//! by [`crate::destination`], and that check refuses the private, loopback and reserved ranges
//! whatever the rules say.
//!
//! The meaning, exactly:
//!
//! - An entry names a site and every name under it: `example.com` covers `example.com` and
//!   `www.example.com`, never `notexample.com`. A leading `*.` means the same (`*.example.com` is
//!   stored as `example.com`). No other wildcard exists.
//! - Entries and hosts are compared in lower case, with one trailing dot dropped. An international
//!   name is written in its `xn--` form, which is what a browser sends; any other non-ASCII entry
//!   is refused, as is a host that isn't a valid name or address.
//! - An entry of `*`, an empty entry, and an entry that is an address are refused when the rules
//!   are set. They never mean "everything".
//! - Block always wins. With [`Mode::All`] every site but the blocked ones is reached; with
//!   [`Mode::Listed`] only the allowed ones that aren't blocked. A bare address matches no name:
//!   with any site blocked it is refused (a blocked site isn't reachable by its number); with none
//!   blocked it is reached under `All` unless something else refuses it, and refused under
//!   `Listed`.
//! - No rules at all (never set, unreadable, malformed) is "refuse every site". It never means
//!   "allow".

use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

/// The only document version this reads.
pub const RULES_VERSION: u8 = 1;
/// Entries in each list, at most.
pub const MAX_ENTRIES: usize = 1000;
/// A rules document's size, at most.
pub const MAX_RULES_BYTES: usize = 128 * 1024;

/// Where the browser's traffic leaves from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Exit {
    /// The box dials the site itself, from its own network.
    Direct,
    /// Through the tunnel's client on the operator's machine. No client attached is no browsing.
    Tunnel,
}

/// Which sites are reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Every site except the blocked ones.
    All,
    /// Only the allowed sites, and never a blocked one.
    Listed,
}

/// A computer's rules, checked and normalised. Made only by [`Rules::parse`] or [`Rules::new`],
/// so every entry in one has passed [`entry`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Rules {
    pub v: u8,
    /// The sender's number for this set, echoed back so it can tell its own set took.
    pub version: u64,
    pub exit: Exit,
    pub mode: Mode,
    pub allow: Vec<String>,
    pub block: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRules {
    v: u8,
    #[serde(default)]
    version: u64,
    exit: Exit,
    mode: Mode,
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    block: Vec<String>,
}

/// Why a rules document or an entry was refused. The text names the list and the position, never
/// the entry itself, so it can be logged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RulesError(pub String);

impl std::fmt::Display for RulesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RulesError {}

/// What the rules say about one CONNECT host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Allowed,
    /// On the block list.
    Blocked,
    /// `Listed` mode, and not on the allow list.
    NotListed,
    /// Not a valid host name or address.
    BadHost,
    /// A bare address while some sites are blocked: a rule names sites, and can't tell which
    /// site an address is, so with a block list an address is refused.
    Address,
}

impl Verdict {
    /// A short, stable word for logs, counts and the `X-Box-Egress-Reason` header.
    pub fn reason(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::Blocked => "blocked",
            Self::NotListed => "not-listed",
            Self::BadHost => "bad-host",
            Self::Address => "address",
        }
    }
}

impl Rules {
    /// Rules from parts, each entry checked as [`Rules::parse`] checks them.
    pub fn new(
        version: u64,
        exit: Exit,
        mode: Mode,
        allow: &[String],
        block: &[String],
    ) -> Result<Self, RulesError> {
        Ok(Self {
            v: RULES_VERSION,
            version,
            exit,
            mode,
            allow: list("allow", allow)?,
            block: list("block", block)?,
        })
    }

    /// Read a rules document (JSON). Anything off, an unknown field included, is an error.
    pub fn parse(bytes: &[u8]) -> Result<Self, RulesError> {
        if bytes.len() > MAX_RULES_BYTES {
            return Err(RulesError(format!(
                "the rules are larger than {MAX_RULES_BYTES} bytes"
            )));
        }
        let raw: RawRules = serde_json::from_slice(bytes)
            .map_err(|err| RulesError(format!("the rules aren't a rules document: {err}")))?;
        if raw.v != RULES_VERSION {
            return Err(RulesError(format!(
                "the rules are version {}, and this reads version {RULES_VERSION}",
                raw.v
            )));
        }
        Self::new(raw.version, raw.exit, raw.mode, &raw.allow, &raw.block)
    }

    /// What these rules say about `host`, the name (or address) the browser asked for.
    pub fn judge(&self, host: &str) -> Verdict {
        let Some(host) = host_of(host) else {
            return Verdict::BadHost;
        };
        let name = match &host {
            Host::Name(name) => Some(name.as_str()),
            Host::Address => None,
        };
        if name.is_some_and(|name| self.block.iter().any(|entry| covers(entry, name))) {
            return Verdict::Blocked;
        }
        if name.is_none() && !self.block.is_empty() {
            return Verdict::Address;
        }
        match self.mode {
            Mode::All => Verdict::Allowed,
            Mode::Listed => {
                if name.is_some_and(|name| self.allow.iter().any(|entry| covers(entry, name))) {
                    Verdict::Allowed
                } else {
                    Verdict::NotListed
                }
            }
        }
    }

    /// The document, as [`Rules::parse`] reads it.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// Counts only: what a status may say about the rules without naming a site.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RulesSummary {
    pub version: u64,
    pub exit: Exit,
    pub mode: Mode,
    pub allow: usize,
    pub block: usize,
}

impl From<&Rules> for RulesSummary {
    fn from(rules: &Rules) -> Self {
        Self {
            version: rules.version,
            exit: rules.exit,
            mode: rules.mode,
            allow: rules.allow.len(),
            block: rules.block.len(),
        }
    }
}

fn list(which: &str, entries: &[String]) -> Result<Vec<String>, RulesError> {
    if entries.len() > MAX_ENTRIES {
        return Err(RulesError(format!(
            "the {which} list has more than {MAX_ENTRIES} entries"
        )));
    }
    let mut out: Vec<String> = Vec::with_capacity(entries.len());
    for (at, raw) in entries.iter().enumerate() {
        let normal =
            entry(raw).map_err(|why| RulesError(format!("{which} entry {}: {why}", at + 1)))?;
        if !out.contains(&normal) {
            out.push(normal);
        }
    }
    Ok(out)
}

/// One list entry, normalised: lower case, no trailing dot, no leading `*.`. The error says why in
/// words a person can act on, and doesn't repeat the entry.
pub fn entry(raw: &str) -> Result<String, &'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("an empty entry would match nothing you meant; name a site");
    }
    if trimmed == "*" || trimmed == "*." {
        return Err("`*` would mean every site; name sites one at a time");
    }
    let name = trimmed.strip_prefix("*.").unwrap_or(trimmed);
    if name.contains('*') {
        return Err("only a leading `*.` is understood, and it means the same as the bare name");
    }
    if !name.is_ascii() {
        return Err("write an international name in its xn-- form");
    }
    if name.contains("://") || name.contains('/') || name.contains(':') {
        return Err("name the site only, with no scheme, port or path (example.com)");
    }
    let name = name.strip_suffix('.').unwrap_or(name).to_ascii_lowercase();
    if !valid_name(&name) {
        return Err("that isn't a site name (letters, digits and hyphens between dots)");
    }
    if last_label_is_numeric(&name) {
        return Err("that is an address, not a name; the lists hold names");
    }
    Ok(name)
}

/// `host` as the rules see it.
enum Host {
    Name(String),
    Address,
}

fn host_of(raw: &str) -> Option<Host> {
    let raw = raw.trim();
    let bare = raw
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(raw);
    if bare.parse::<std::net::IpAddr>().is_ok() {
        return Some(Host::Address);
    }
    if !raw.is_ascii() {
        return None;
    }
    let name = raw.strip_suffix('.').unwrap_or(raw).to_ascii_lowercase();
    if !valid_name(&name) {
        return None;
    }
    if last_label_is_numeric(&name) {
        // Not a name and not an address either (`1.2.3`, `999.1.1.1`).
        return None;
    }
    Some(Host::Name(name))
}

fn valid_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 253 {
        return false;
    }
    name.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    })
}

fn last_label_is_numeric(name: &str) -> bool {
    name.rsplit('.')
        .next()
        .is_some_and(|last| last.bytes().all(|b| b.is_ascii_digit()))
}

/// `entry` covers `name` when it is the name or a domain above it.
pub(crate) fn covers(entry: &str, name: &str) -> bool {
    name == entry
        || (name.len() > entry.len()
            && name.ends_with(entry)
            && name.as_bytes()[name.len() - entry.len() - 1] == b'.')
}

/// The rules in force, shared between whoever sets them and every stream that reads them. `None`
/// is "no rules", which refuses every site.
#[derive(Clone, Debug, Default)]
pub struct RulesCell(Arc<RwLock<Option<Arc<Rules>>>>);

impl RulesCell {
    pub fn new(rules: Option<Rules>) -> Self {
        Self(Arc::new(RwLock::new(rules.map(Arc::new))))
    }

    pub fn get(&self) -> Option<Arc<Rules>> {
        self.0.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set(&self, rules: Option<Rules>) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = rules.map(Arc::new);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(mode: Mode, allow: &[&str], block: &[&str]) -> Rules {
        let allow: Vec<String> = allow.iter().map(|s| s.to_string()).collect();
        let block: Vec<String> = block.iter().map(|s| s.to_string()).collect();
        Rules::new(1, Exit::Direct, mode, &allow, &block).unwrap()
    }

    #[test]
    fn a_name_covers_itself_and_every_name_under_it_and_nothing_else() {
        let r = rules(Mode::All, &[], &["facebook.com"]);
        assert_eq!(r.judge("facebook.com"), Verdict::Blocked);
        assert_eq!(r.judge("www.facebook.com"), Verdict::Blocked);
        assert_eq!(r.judge("a.b.facebook.com"), Verdict::Blocked);
        assert_eq!(r.judge("notfacebook.com"), Verdict::Allowed);
        assert_eq!(r.judge("facebook.com.evil.test"), Verdict::Allowed);
        assert_eq!(r.judge("com"), Verdict::Allowed);
    }

    #[test]
    fn case_and_one_trailing_dot_do_not_matter() {
        let r = rules(Mode::All, &[], &["FaceBook.COM."]);
        assert_eq!(r.block, vec!["facebook.com".to_string()]);
        assert_eq!(r.judge("WWW.Facebook.Com."), Verdict::Blocked);
        assert_eq!(r.judge("facebook.com"), Verdict::Blocked);
        assert_eq!(r.judge("facebook.com.."), Verdict::BadHost);
    }

    #[test]
    fn a_leading_star_dot_means_the_bare_name() {
        let r = rules(Mode::Listed, &["*.example.com", "example.com"], &[]);
        assert_eq!(r.allow, vec!["example.com".to_string()]);
        assert_eq!(r.judge("example.com"), Verdict::Allowed);
        assert_eq!(r.judge("shop.example.com"), Verdict::Allowed);
    }

    #[test]
    fn block_wins_over_allow() {
        let r = rules(Mode::Listed, &["example.com"], &["ads.example.com"]);
        assert_eq!(r.judge("example.com"), Verdict::Allowed);
        assert_eq!(r.judge("ads.example.com"), Verdict::Blocked);
        assert_eq!(r.judge("x.ads.example.com"), Verdict::Blocked);
        let r = rules(Mode::Listed, &["example.com"], &["example.com"]);
        assert_eq!(r.judge("example.com"), Verdict::Blocked);
    }

    #[test]
    fn listed_mode_refuses_whatever_is_not_allowed_and_bare_addresses() {
        let r = rules(Mode::Listed, &["example.com"], &[]);
        assert_eq!(r.judge("example.org"), Verdict::NotListed);
        assert_eq!(r.judge("93.184.216.34"), Verdict::NotListed);
        assert_eq!(r.judge("[2606:2800:220:1::1]"), Verdict::NotListed);
        let empty = rules(Mode::Listed, &[], &[]);
        assert_eq!(empty.judge("example.com"), Verdict::NotListed);
    }

    #[test]
    fn a_bare_address_is_reached_only_when_no_site_is_blocked() {
        let open = rules(Mode::All, &[], &[]);
        assert_eq!(open.judge("93.184.216.34"), Verdict::Allowed);
        assert_eq!(open.judge("[2606:2800:220:1::1]"), Verdict::Allowed);
        let blocking = rules(Mode::All, &[], &["example.com"]);
        assert_eq!(
            blocking.judge("93.184.216.34"),
            Verdict::Address,
            "a blocked site isn't reached by its number"
        );
        assert_eq!(blocking.judge("::1"), Verdict::Address);
        assert_eq!(blocking.judge("other.example"), Verdict::Allowed);
    }

    #[test]
    fn a_star_an_empty_entry_and_an_address_are_refused_and_never_mean_everything() {
        for bad in ["*", " ", "", "*.", "*.*.com", "ex*ample.com"] {
            let err = Rules::new(1, Exit::Direct, Mode::All, &[], &[bad.to_string()]);
            assert!(err.is_err(), "{bad:?} must be refused");
            let err = Rules::new(1, Exit::Direct, Mode::Listed, &[bad.to_string()], &[]);
            assert!(err.is_err(), "{bad:?} must be refused");
        }
        for bad in [
            "1.2.3.4",
            "10.0.0.1",
            "::1",
            "[::1]",
            "https://example.com",
            "example.com/path",
            "example.com:443",
            "exa mple.com",
            "-example.com",
            "bücher.de",
        ] {
            assert!(entry(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn an_error_names_the_list_and_position_and_not_the_entry() {
        let err = Rules::new(
            1,
            Exit::Direct,
            Mode::All,
            &["ok.test".into(), "*".into()],
            &[],
        )
        .unwrap_err();
        assert!(err.0.starts_with("allow entry 2:"), "{err}");
    }

    #[test]
    fn international_names_are_matched_in_their_xn_form() {
        let r = rules(Mode::Listed, &["xn--bcher-kva.de"], &[]);
        assert_eq!(r.judge("www.xn--bcher-kva.de"), Verdict::Allowed);
        assert_eq!(r.judge("www.bücher.de"), Verdict::BadHost);
    }

    #[test]
    fn single_label_names_are_names() {
        let r = rules(Mode::Listed, &["localhost"], &["blocked"]);
        assert_eq!(r.judge("localhost"), Verdict::Allowed);
        assert_eq!(r.judge("blocked"), Verdict::Blocked);
    }

    #[test]
    fn a_document_reads_strictly_and_round_trips() {
        let r = Rules::parse(
            br#"{"v":1,"version":7,"exit":"tunnel","mode":"listed","allow":["Example.com"],"block":[]}"#,
        )
        .unwrap();
        assert_eq!(r.version, 7);
        assert_eq!(r.exit, Exit::Tunnel);
        assert_eq!(r.allow, vec!["example.com".to_string()]);
        assert_eq!(Rules::parse(r.to_json().as_bytes()).unwrap(), r);
        for bad in [
            &br#"{"v":2,"exit":"direct","mode":"all"}"#[..],
            br#"{"v":1,"exit":"direct","mode":"all","extra":1}"#,
            br#"{"v":1,"exit":"sideways","mode":"all"}"#,
            br#"{"v":1,"exit":"direct","mode":"everything"}"#,
            br#"{"v":1,"exit":"direct"}"#,
            br#"{"v":1,"exit":"direct","mode":"all","allow":["*"]}"#,
            b"",
            b"not json",
        ] {
            assert!(
                Rules::parse(bad).is_err(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn too_many_entries_are_refused() {
        let many: Vec<String> = (0..=MAX_ENTRIES).map(|i| format!("s{i}.test")).collect();
        assert!(Rules::new(1, Exit::Direct, Mode::All, &many, &[]).is_err());
    }

    #[test]
    fn an_empty_cell_is_no_rules() {
        let cell = RulesCell::default();
        assert!(cell.get().is_none());
        cell.set(Some(rules(Mode::All, &[], &[])));
        assert!(cell.get().is_some());
        cell.set(None);
        assert!(cell.get().is_none());
    }
}
