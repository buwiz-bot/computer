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
pub mod listen;
pub mod request_id;

pub use alloc::GLOBAL_ALLOCATOR;
pub use auth::{bearer_token, parse_bearer, tokens_equal};
pub use config::{
    ensure_token_strength, read_secret_from_env, token_is_insecure, wipe_secret_environ, BoxConfig,
    ConfigError, MIN_TOKEN_LEN, SECRET_ENV_KEYS, SECRET_FILE_ENV_KEYS,
};
pub use cors::layer as cors_layer;
pub use env::{env_bool, env_nonempty};
pub use error::{ApiError, ErrorBody, ErrorResponse};
pub use jail::{resolve_in_canonical_jail, resolve_in_jail, JailError};
pub use listen::{container_local_host, container_local_http_url};
pub use request_id::{echo_request_id, new_id, new_prefixed_id, RequestId, REQUEST_ID_HEADER};

/// Protocol version advertised by both daemons.
pub const PROTOCOL_VERSION: &str = "v1";

/// Well-known insecure token. Rejected unless `BOX_ALLOW_INSECURE_DEV=1`
/// and both daemon binds are loopback.
pub const DEV_BOX_TOKEN: &str = "dev-box-token";

/// Take this process out of reach of the others running as its user.
///
/// A dumpable process's memory is open to any process of the same uid, through
/// `/proc/<pid>/mem` or ptrace, and everything in the box runs as one uid, exec children included.
/// With this off, the kernel refuses them both. Call it first thing in any process that will hold
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
