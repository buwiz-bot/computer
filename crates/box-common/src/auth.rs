use http::header::{HeaderMap, AUTHORIZATION};
use subtle::ConstantTimeEq;

/// Extract a Bearer token from an `Authorization` header map.
pub fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    parse_bearer(value)
}

/// Parse `Bearer <token>` (case-insensitive scheme).
pub fn parse_bearer(value: &str) -> Option<&str> {
    let value = value.trim();
    let (scheme, rest) = value.split_once(char::is_whitespace)?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = rest.trim();
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

/// Constant-time equality for bearer tokens.
///
/// A token of the wrong length is refused at once, so the time taken says whether the length was
/// right. That is by design: the length of a generated token is not secret. Tokens of the same
/// length are compared without a short-circuiting byte compare, so the time says nothing about
/// their bytes.
pub fn tokens_equal(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    bool::from(left.ct_eq(right))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    #[test]
    fn parses_bearer() {
        assert_eq!(parse_bearer("Bearer secret"), Some("secret"));
        assert_eq!(parse_bearer("bearer secret"), Some("secret"));
        assert_eq!(parse_bearer("Bearer  secret  "), Some("secret"));
        assert_eq!(parse_bearer("Basic secret"), None);
        assert_eq!(parse_bearer("Bearer"), None);
        assert_eq!(parse_bearer(""), None);
    }

    #[test]
    fn header_extract() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer abc"));
        assert_eq!(bearer_token(&headers), Some("abc"));
    }

    #[test]
    fn reject_wrong_token() {
        assert!(!tokens_equal("dev-box-token", "wrong"));
        assert!(!tokens_equal("short", "longer-token"));
        assert!(tokens_equal("dev-box-token", "dev-box-token"));
    }
}
