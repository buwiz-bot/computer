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
///
/// A glob matches the whole label, case and whitespace aside (and zero-width characters), so
/// `Order*` is a label that starts with "Order": it asks about "Order now", and about "Order
/// history" and "Orders" too, as `Remove*` does about "Remove filter" and `Checkout*` about a
/// button that only opens the checkout. A press asked about by mistake costs a person a look; one
/// not asked about can't be taken back. "Reorder" or "Complete purchase" aren't caught: a label
/// must start with the word.
pub const DEFAULT_CONFIRM_PRESS: &str = "Pay*,Delete*,Buy*,Place order*,Checkout*,Submit order*,Confirm purchase*,Remove*,Order*,Purchase*,Subscribe*";
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

    /// May the browser go to `url`, or give input on a page there? While there's a list, only an
    /// `http:`/`https:` address on a listed host, or `about:blank`, may. The scheme decides first:
    /// any other address (`data:`, `file:`, `blob:`, `view-source:`, `javascript:`, `chrome:`, a
    /// string with no scheme) is refused whatever its text holds, so `data:…,https://shop.example/`
    /// is not on shop.example. `blob:https://shop.example/…` and `view-source:https://…` are
    /// refused too: their content isn't the listed site's page.
    pub fn check(&self, url: Option<&str>) -> Result<(), BrowserError> {
        if self.hosts.is_empty() {
            return Ok(());
        }
        let Some(url) = url else {
            return Err(BrowserError::DomainNotAllowed(
                "the page's address is unknown".into(),
            ));
        };
        match web_host(url) {
            None if is_blank(url) => Ok(()),
            None => Err(BrowserError::DomainNotAllowed(no_host(url))),
            Some(host) if self.hosts.iter().any(|p| p.matches(&host)) => Ok(()),
            Some(host) => Err(BrowserError::DomainNotAllowed(host)),
        }
    }
}

/// The host of an `http:` or `https:` address (scheme in any case, after a trim), read the way
/// ultra-instinct's `host_of` reads it (userinfo and port dropped); `None` for every other scheme,
/// and for an `http(s):` address without `//` right after the scheme.
fn web_host(url: &str) -> Option<String> {
    let url = url.trim();
    let (scheme, rest) = url.split_once(':')?;
    let web = scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https");
    if !web || !rest.starts_with("//") {
        return None;
    }
    host_of(url)
}

/// What a refusal names for an address with no host to match, in ultra-instinct's words
/// (`refused_place`), so a caller reads one phrasing whichever list refused: `a blob: page`,
/// `a data: page`, `an https: URL with no host`, or `(unknown url)` for a string with no scheme.
/// The address itself is never echoed.
fn no_host(url: &str) -> String {
    let url = url.trim();
    let scheme = url.split_once(':').map(|(s, _)| s).filter(|s| {
        let mut chars = s.chars();
        chars.next().is_some_and(|c| c.is_ascii_alphabetic())
            && chars.all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    });
    match scheme.map(str::to_ascii_lowercase) {
        Some(web) if web == "http" || web == "https" => format!("an {web}: URL with no host"),
        Some(scheme) if scheme.starts_with(['a', 'e', 'i', 'o', 'u']) => {
            format!("an {scheme}: page")
        }
        Some(scheme) => format!("a {scheme}: page"),
        None => "(unknown url)".into(),
    }
}

/// Whether two labels are the same label as ultra-instinct matches them: invisible characters
/// (zero-width spaces and joiners, a soft hyphen, …) dropped and whitespace folded on both sides
/// (`ultra_instinct_core::normalize_label`). Case counts: a label whose case changed is asked
/// about again, which is the stricter way.
pub fn same_label(a: &str, b: &str) -> bool {
    a == b || ultra_instinct_core::normalize_label(a) == ultra_instinct_core::normalize_label(b)
}

/// `about:blank`, with or without a fragment or a query: the one hostless page a list lets in.
fn is_blank(url: &str) -> bool {
    let url = url.trim();
    let rest = url
        .get(..11)
        .filter(|head| head.eq_ignore_ascii_case("about:blank"))
        .map(|_| &url[11..]);
    matches!(rest, Some(r) if r.is_empty() || r.starts_with(['#', '?']))
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
    fn the_default_globs_catch_paying_ordering_deleting_and_subscribing() {
        let rules = Rules::default();
        assert_eq!(rules.globs().len(), 11);
        for label in [
            "Pay $18",
            "Delete account",
            "Buy now",
            "Place order",
            "Checkout",
            "Checkout now",
            "Submit order",
            "Confirm purchase",
            "Remove from cart",
            "Order now",
            "Purchase",
            "Subscribe for $9.99/month",
            // case, and space around and inside, don't matter
            "  PLACE   ORDER ",
            "checkout",
        ] {
            assert!(rules.needs_confirm(label), "{label}");
        }
        // the price of a short list of words: a label that only starts like one is asked about too
        for label in [
            "Order history",
            "Orders",
            "Remove filter",
            "Purchase history",
        ] {
            assert!(rules.needs_confirm(label), "{label}");
        }
        // a label must start with the word
        for label in [
            "Search",
            "Sign in",
            "Add to cart",
            "Reorder",
            "Complete purchase",
            "View order",
            "Pa y",
        ] {
            assert!(!rules.needs_confirm(label), "{label}");
        }
        assert!(Rules::parse("").unwrap().globs().is_empty());
    }

    /// A character that renders as nothing doesn't hide a label from the globs, in the label or in
    /// a glob from the environment; the same label with one is the same label.
    #[test]
    fn an_invisible_character_hides_nothing_from_the_confirm_rules() {
        let rules = Rules::default();
        for label in [
            "Pa\u{200B}y $18",
            "\u{FEFF}Pay $18",
            "P\u{AD}a\u{34F}y\u{2060} $18",
            "Place\u{200C} \u{200D}order",
            "  pay   $18 ",
        ] {
            assert!(rules.needs_confirm(label), "{label:?}");
        }
        let from_env = Rules::parse("Pa\u{200B}y*").unwrap();
        assert!(from_env.needs_confirm("Pay $18"));
        assert!(same_label("Pa\u{200B}y $18", "Pay $18"));
        assert!(same_label("Pay  $18", "Pay $18"));
        assert!(!same_label("Pay $18", "Pay $19"));
        assert!(!same_label("pay $18", "Pay $18"));
    }

    #[test]
    fn an_allow_list_lets_its_hosts_and_about_blank_through() {
        let list =
            AllowList::parse(Some(&["shop.example".into(), "*.bank.example".into()])).unwrap();
        assert!(list.check(Some("https://shop.example/cart")).is_ok());
        assert!(list.check(Some("https://www.bank.example/")).is_ok());
        assert!(list.check(Some("about:blank")).is_ok());
        assert!(list.check(Some("about:blank#top")).is_ok());
        // a page with no host to check is off a list
        for hostless in [
            "file:///etc/passwd",
            "data:text/html,<b>hi</b>",
            "about:blankety",
        ] {
            assert!(
                matches!(
                    list.check(Some(hostless)),
                    Err(BrowserError::DomainNotAllowed(_))
                ),
                "{hostless}"
            );
        }
        assert!(matches!(
            list.check(Some("https://evil.example/")),
            Err(BrowserError::DomainNotAllowed(host)) if host == "evil.example"
        ));
        assert!(list.check(None).is_err());
        assert!(AllowList::parse(None).unwrap().check(None).is_ok());
        // a page with no host is named by its scheme, never by the address it holds
        for (url, named) in [
            ("data:text/html,<b>secret</b>", "a data: page"),
            (" BLOB:https://shop.example/0b7c", "a blob: page"),
            ("file:///etc/passwd", "a file: page"),
            ("about:srcdoc", "an about: page"),
            ("https:shop.example", "an https: URL with no host"),
            ("//shop.example/", "(unknown url)"),
            ("", "(unknown url)"),
        ] {
            assert!(
                matches!(list.check(Some(url)), Err(BrowserError::DomainNotAllowed(h)) if h == named),
                "{url}"
            );
        }
        assert!(AllowList::parse(Some(&["not a host".into()])).is_err());
    }

    #[test]
    fn only_a_web_address_has_a_host_to_match() {
        let list = AllowList::parse(Some(&["shop.example".into()])).unwrap();
        for ok in [
            "https://shop.example/",
            "HTTPS://shop.example/cart",
            "Http://shop.example",
            "  https://shop.example/  ",
            "https://user:pw@shop.example:8443/x",
            " ABOUT:BLANK?x",
        ] {
            assert!(list.check(Some(ok)).is_ok(), "{ok}");
        }
        for refused in [
            "data:text/html,https://shop.example/",
            "DATA:text/html;base64,aHR0cHM6Ly9zaG9wLmV4YW1wbGUv,https://shop.example/",
            "  data:,https://shop.example/",
            "blob:https://shop.example/0b7c6a3e-1d2f-4c1a-9d9e-3f1e2a7b6c5d",
            "view-source:https://shop.example/",
            "javascript:'https://shop.example/'",
            "file://shop.example/etc/passwd",
            "chrome://shop.example/",
            "ftp://shop.example/",
            "shop.example",
            "//shop.example/",
            "https:shop.example",
            "https:/x://shop.example/",
            "",
        ] {
            assert!(
                matches!(
                    list.check(Some(refused)),
                    Err(BrowserError::DomainNotAllowed(_))
                ),
                "{refused}"
            );
        }
        // userinfo is not the host
        assert!(matches!(
            list.check(Some("https://shop.example@evil.example/")),
            Err(BrowserError::DomainNotAllowed(host)) if host == "evil.example"
        ));
    }

    /// A `blob:` page is off a list whatever its origin, since its content is whatever the page made
    /// it, not the listed site's page. (ultra-instinct's guard refuses it too, with the policy
    /// box-browser gives it: `seam`.)
    #[test]
    fn a_blob_page_is_off_a_list_even_on_a_listed_origin() {
        let list =
            AllowList::parse(Some(&["shop.example".into(), "*.shop.example".into()])).unwrap();
        for blob in [
            "blob:https://shop.example/0b7c6a3e-1d2f-4c1a-9d9e-3f1e2a7b6c5d",
            "BLOB:https://shop.example/0b7c6a3e",
            "  blob:http://www.shop.example/x",
            "blob:about:blank",
        ] {
            assert!(
                matches!(
                    list.check(Some(blob)),
                    Err(BrowserError::DomainNotAllowed(_))
                ),
                "{blob}"
            );
        }
        // named as ultra-instinct names one: the same words whichever list refused it
        assert!(matches!(
            list.check(Some("blob:https://shop.example/x")),
            Err(BrowserError::DomainNotAllowed(h))
                if h == ultra_instinct_guard::refused_place("blob:https://shop.example/x")
        ));
        // with no list, nothing is checked
        assert!(AllowList::parse(None)
            .unwrap()
            .check(Some("blob:https://shop.example/x"))
            .is_ok());
    }

    #[test]
    fn a_token_is_good_once_for_its_target() {
        let mut confirms = Confirms::default();
        let token = confirms.issue("Pay $18", Some("n100")).unwrap();
        assert_eq!(token.len(), 32);
        assert_eq!(confirms.peek(&token, Some("n200")), None);
        // a look leaves it good
        for _ in 0..2 {
            assert_eq!(
                confirms.peek(&token, Some("n100")).as_deref(),
                Some("Pay $18")
            );
        }
        confirms.spend(&token);
        assert_eq!(confirms.peek(&token, Some("n100")), None);
        assert_eq!(confirms.peek("guess", Some("n100")), None);
    }
}
