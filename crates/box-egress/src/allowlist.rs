//! Optional CONNECT / relay host allowlist (`BOX_EGRESS_RELAY_HOSTS`).

/// Hostname patterns. An empty list places no *name* restriction -- but it no
/// longer means "dial anything": every destination is additionally judged on its
/// resolved address by [`crate::destination::DestinationPolicy`], which refuses
/// loopback, private, link-local and other non-public addresses unless someone
/// explicitly opts in. Matching here is on the CONNECT host string, which the
/// guest chooses, so treat it as a convenience filter and not as the boundary.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Allowlist {
    /// Names, each covering itself and every name under it, by the rules' own matcher: the same
    /// string means the same thing in the rules and here. Empty is no filter.
    names: Vec<String>,
}

impl Allowlist {
    pub fn any() -> Self {
        Self { names: Vec::new() }
    }

    /// Comma-separated names; `*.x` is a synonym of `x`. Unset or empty is no filter, and `*`
    /// alone in it is no filter on purpose. A value that was given and names nothing usable
    /// (`*.`, `*..`, a lone comma) is an error: a typo must never become "every name".
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(Self::any());
        }
        let mut names = Vec::new();
        for part in raw.split(',').map(str::trim) {
            if part == "*" {
                return Ok(Self::any());
            }
            let name = part
                .strip_prefix("*.")
                .unwrap_or(part)
                .trim_matches('.')
                .to_ascii_lowercase();
            if name.is_empty() || name.contains('*') {
                continue;
            }
            if !names.contains(&name) {
                names.push(name);
            }
        }
        if names.is_empty() {
            return Err(
                "names no host it could match (write a name, like example.com, or leave it unset)"
                    .into(),
            );
        }
        Ok(Self { names })
    }

    pub fn is_open(&self) -> bool {
        self.names.is_empty()
    }

    pub fn allows(&self, host: &str) -> bool {
        if self.names.is_empty() {
            return true;
        }
        let host = host.trim();
        let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
        if host.is_empty() {
            return false;
        }
        self.names
            .iter()
            .any(|name| crate::rules::covers(name, &host))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_allows_all() {
        let a = Allowlist::parse("").unwrap();
        assert!(a.is_open());
        assert!(a.allows("anything.example"));
    }

    #[test]
    fn a_name_covers_itself_and_its_subdomains_as_in_the_rules() {
        let a = Allowlist::parse("facebook.com, *.fbcdn.net").unwrap();
        assert!(a.allows("facebook.com"));
        assert!(a.allows("Facebook.COM."));
        assert!(
            a.allows("www.facebook.com"),
            "the same string means the same as a rules entry"
        );
        assert!(!a.allows("evil-facebook.com"));
        assert!(a.allows("foo.fbcdn.net"));
        assert!(a.allows("fbcdn.net"));
        assert!(!a.allows("example.com"));
        let rules = crate::rules::Rules::new(
            1,
            crate::rules::Exit::Direct,
            crate::rules::Mode::Listed,
            &["facebook.com".into()],
            &[],
        )
        .unwrap();
        for host in ["facebook.com", "www.facebook.com", "notfacebook.com"] {
            assert_eq!(
                a.allows(host),
                rules.judge(host) == crate::rules::Verdict::Allowed,
                "{host}"
            );
        }
    }

    #[test]
    fn star_is_open() {
        let a = Allowlist::parse("*, facebook.com").unwrap();
        assert!(a.is_open());
        assert!(a.allows("elsewhere.test"));
    }

    #[test]
    fn a_value_that_names_nothing_is_an_error_never_an_open_list() {
        for bad in ["*.", "*..", " *. ", "*.,*.", ",", " , ", "*.*"] {
            assert!(Allowlist::parse(bad).is_err(), "{bad:?}");
        }
    }
}
