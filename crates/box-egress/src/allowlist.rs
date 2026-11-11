//! Optional CONNECT / relay host allowlist (`BOX_EGRESS_RELAY_HOSTS`).

/// Hostname patterns. An empty list places no *name* restriction -- but it no
/// longer means "dial anything": every destination is additionally judged on its
/// resolved address by [`crate::destination::DestinationPolicy`], which refuses
/// loopback, private, link-local and other non-public addresses unless someone
/// explicitly opts in. Matching here is on the CONNECT host string, which the
/// guest chooses, so treat it as a convenience filter and not as the boundary.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Allowlist {
    patterns: Vec<Pattern>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Pattern {
    Exact(String),
    /// `*.example.com` also matches `example.com`.
    Suffix(String),
}

impl Allowlist {
    pub fn any() -> Self {
        Self {
            patterns: Vec::new(),
        }
    }

    /// Comma-separated list. `*` (alone) or an empty string means allow all.
    pub fn parse(raw: &str) -> Self {
        let mut patterns = Vec::new();
        for part in raw.split(',') {
            let p = part.trim();
            if p.is_empty() {
                continue;
            }
            if p == "*" {
                return Self::any();
            }
            if let Some(suffix) = p.strip_prefix("*.") {
                let suffix = suffix.trim_matches('.').to_ascii_lowercase();
                if !suffix.is_empty() {
                    patterns.push(Pattern::Suffix(suffix));
                }
            } else {
                patterns.push(Pattern::Exact(p.trim_matches('.').to_ascii_lowercase()));
            }
        }
        Self { patterns }
    }

    pub fn is_open(&self) -> bool {
        self.patterns.is_empty()
    }

    pub fn allows(&self, host: &str) -> bool {
        if self.patterns.is_empty() {
            return true;
        }
        let host = host.trim().trim_matches('.').to_ascii_lowercase();
        if host.is_empty() {
            return false;
        }
        self.patterns.iter().any(|pattern| match pattern {
            Pattern::Exact(exact) => host == *exact,
            Pattern::Suffix(suffix) => host == *suffix || host.ends_with(&format!(".{suffix}")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_allows_all() {
        let a = Allowlist::parse("");
        assert!(a.is_open());
        assert!(a.allows("anything.example"));
    }

    #[test]
    fn exact_and_wildcard() {
        let a = Allowlist::parse("facebook.com, *.fbcdn.net");
        assert!(a.allows("facebook.com"));
        assert!(a.allows("Facebook.COM"));
        assert!(!a.allows("evil-facebook.com"));
        assert!(a.allows("foo.fbcdn.net"));
        assert!(a.allows("fbcdn.net"));
        assert!(!a.allows("example.com"));
    }

    #[test]
    fn star_is_open() {
        let a = Allowlist::parse("*, facebook.com");
        assert!(a.is_open());
        assert!(a.allows("elsewhere.test"));
    }
}
