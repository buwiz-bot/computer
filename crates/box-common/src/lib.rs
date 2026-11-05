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
