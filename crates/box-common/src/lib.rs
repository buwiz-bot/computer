//! Shared primitives for buwiz-computer daemons.
//!
//! This crate is the only place path jail and bearer-token comparison live so
//! `box-exec` and `box-host` cannot drift.

mod alloc;
pub mod auth;
pub mod config;
pub mod cors;
pub mod env;
pub mod error;
pub mod jail;
pub mod json;
pub mod listen;
pub mod local_http;
pub mod peer;
pub mod process;
pub mod request_id;

pub use alloc::GLOBAL_ALLOCATOR;
pub use auth::{bearer_token, parse_bearer, tokens_equal};
pub use config::{
    ensure_not_placeholder, ensure_token_strength, ensure_viewer_password_strength, parse_addr,
    read_secret_from_env, token_is_insecure, wipe_secret_environ, BoxConfig, ConfigError,
    MIN_TOKEN_LEN, PLACEHOLDER_MARKER, SECRET_ENV_KEYS, SECRET_FILE_ENV_KEYS,
};
pub use cors::layer as cors_layer;
pub use env::{env_bool, env_nonempty};
pub use error::{ApiError, ErrorBody, ErrorResponse};
pub use jail::{resolve_in_canonical_jail, resolve_in_jail, JailError};
pub use listen::{container_local_host, container_local_http_url};
pub use local_http::{local_get, LocalResponse};
pub use request_id::{echo_request_id, new_id, new_prefixed_id, RequestId, REQUEST_ID_HEADER};

/// Where `box-keeper` takes screen requests. Its user alone can open it.
pub const KEEPER_SOCKET: &str = "/run/box/keeper.sock";

/// A Bot id names folders and a profile: letters, digits, `_` and `-` only. `main` and `shared` are
/// the box's own folders.
pub fn valid_bot(bot: &str) -> bool {
    (1..=80).contains(&bot.len())
        && !matches!(bot, "main" | "shared")
        && bot
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// One request to `box-screen`, as it crosses from `box-exec` to `box-keeper`. Both ends check it,
/// and what's run is these words as arguments, with no shell between.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenCall(Vec<String>);

impl ScreenCall {
    pub fn new(action: &str, bot: Option<&str>, url: Option<&str>) -> Option<Self> {
        let word = |w: &str| w.to_string();
        match (action, bot, url) {
            ("list", None, None) => Some(Self(vec![word(action)])),
            ("up" | "down" | "forget", Some(bot), None) if valid_bot(bot) => {
                Some(Self(vec![word(action), word(bot)]))
            }
            ("open", Some(bot), Some(url)) if valid_bot(bot) && page(url) => {
                Some(Self(vec![word(action), word(bot), word(url)]))
            }
            _ => None,
        }
    }

    /// What [`Self::line`] wrote.
    pub fn parse(line: &str) -> Option<Self> {
        let mut words = line.trim_end_matches('\n').split(' ');
        let (action, bot, url) = (words.next()?, words.next(), words.next());
        words
            .next()
            .is_none()
            .then(|| Self::new(action, bot, url))?
    }

    pub fn line(&self) -> String {
        format!("{}\n", self.0.join(" "))
    }

    pub fn args(&self) -> &[String] {
        &self.0
    }
}

/// An http or https address that can't be read as a flag or split into two words.
fn page(url: &str) -> bool {
    (url.starts_with("http://") || url.starts_with("https://"))
        && url.len() <= 8192
        && url.bytes().all(|b| b.is_ascii_graphic() || b >= 0x80)
}

/// Protocol version advertised by both daemons.
pub const PROTOCOL_VERSION: &str = "v1";

/// Well-known insecure token. Rejected unless `BOX_ALLOW_INSECURE_DEV=1`
/// and both daemon binds are loopback.
pub const DEV_BOX_TOKEN: &str = "dev-box-token";

/// Take this process out of reach of the others running as its user.
///
/// A dumpable process's memory is open to any process of the same uid, through
/// `/proc/<pid>/mem` or ptrace. With this off, the kernel refuses them both. Call it first thing in any process that will hold
/// a secret. It does not survive `execve`, so a child starts dumpable as usual.
pub fn make_undumpable() {
    #[cfg(target_os = "linux")]
    // SAFETY: PR_SET_DUMPABLE takes an integer and touches no memory of ours.
    unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 0);
    }
}

/// True when `buf` holds a whole HTTP response: its headers and `Content-Length` bytes of body.
/// Chromium's DevTools server speaks only HTTP/1.1 and leaves the socket open even after
/// `Connection: close`, so reading to the end of the stream would only ever time out.
pub fn http_response_complete(buf: &[u8]) -> bool {
    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return false;
    };
    let headers = String::from_utf8_lossy(&buf[..end]);
    let length = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())?
    });
    length.is_some_and(|length| buf.len() >= end + 4 + length)
}

#[cfg(test)]
mod http_tests {
    use super::http_response_complete;

    #[test]
    fn a_response_is_complete_at_its_content_length() {
        assert!(!http_response_complete(
            b"HTTP/1.1 200 OK\r\nContent-Length:4"
        ));
        assert!(!http_response_complete(
            b"HTTP/1.1 200 OK\r\nContent-Length:4\r\n\r\nabc"
        ));
        assert!(http_response_complete(
            b"HTTP/1.1 200 OK\r\nContent-Length:4\r\n\r\nabcd"
        ));
        assert!(http_response_complete(
            b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n"
        ));
    }

    #[test]
    fn with_no_length_only_the_end_of_the_stream_can_say() {
        assert!(!http_response_complete(b"HTTP/1.1 200 OK\r\n\r\nabcd"));
    }
}

#[cfg(test)]
mod screen_call_tests {
    use super::*;

    #[test]
    fn a_call_survives_the_socket() {
        for call in [
            ScreenCall::new("up", Some("amy_1"), None),
            ScreenCall::new("forget", Some("amy-1"), None),
            ScreenCall::new("list", None, None),
            ScreenCall::new("open", Some("amy"), Some("https://a.example/x?y=1")),
        ] {
            let call = call.unwrap();
            assert_eq!(ScreenCall::parse(&call.line()), Some(call));
        }
    }

    #[test]
    fn nothing_else_gets_through() {
        for line in [
            "",
            "up",
            "rm -rf /",
            "up ../x",
            "up main",
            "up shared",
            "up amy extra",
            "open amy --no-sandbox",
            "open amy file:///etc/passwd",
            "open amy https://a b",
            "list amy",
        ] {
            assert_eq!(ScreenCall::parse(line), None, "{line:?}");
        }
        assert_eq!(
            ScreenCall::new("open", Some("amy"), Some("https://a\nb")),
            None
        );
    }
}
