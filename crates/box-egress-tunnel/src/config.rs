//! Bind addresses, bearer, and allowlist for the tunnel process.

use std::net::SocketAddr;
use std::path::PathBuf;

use box_common::{
    ensure_token_strength, env_bool, env_nonempty, parse_addr, read_secret_from_env,
    wipe_secret_environ, ConfigError,
};

use crate::allowlist::Allowlist;
use crate::destination::DestinationPolicy;

const DEFAULT_WS: &str = "127.0.0.1:8790";
const DEFAULT_PROXY: &str = "127.0.0.1:8791";
const DEFAULT_ADMIN: &str = "127.0.0.1:8792";

#[derive(Clone, Debug)]
pub struct TunnelServerConfig {
    pub ws_bind: SocketAddr,
    pub proxy_bind: SocketAddr,
    pub admin_bind: SocketAddr,
    pub bearer: String,
    pub allowlist: Allowlist,
}

#[derive(Clone, Debug)]
pub struct TunnelClientConfig {
    pub url: String,
    pub bearer: String,
    pub allowlist: Allowlist,
    /// Resolved-address and port policy applied immediately before each dial.
    pub destination: DestinationPolicy,
    pub reconnect: bool,
}

impl TunnelServerConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        let ws_bind = parse_addr("BOX_EGRESS_WS_BIND", DEFAULT_WS)?;
        let proxy_bind = parse_addr("BOX_EGRESS_PROXY_BIND", DEFAULT_PROXY)?;
        let admin_bind = parse_addr("BOX_EGRESS_ADMIN_BIND", DEFAULT_ADMIN)?;
        let bearer = read_secret_from_env("BOX_EGRESS_TUNNEL_BEARER", true)?.ok_or_else(|| {
            ConfigError(
                "BOX_EGRESS_TUNNEL_BEARER must be set and non-empty, or BOX_EGRESS_TUNNEL_BEARER_FILE must point at a file containing it"
                    .into(),
            )
        })?;
        ensure_token_strength(
            "BOX_EGRESS_TUNNEL_BEARER",
            &bearer,
            ws_bind.ip().is_loopback(),
        )?;
        if !proxy_bind.ip().is_loopback() {
            tracing::warn!(
                %proxy_bind,
                "CONNECT proxy bind is not loopback; do not publish this port"
            );
        }
        if !admin_bind.ip().is_loopback() {
            tracing::warn!(
                %admin_bind,
                "admin bind is not loopback; status is unauthenticated"
            );
        }
        let allowlist =
            Allowlist::parse(&env_nonempty("BOX_EGRESS_RELAY_HOSTS").unwrap_or_default());
        wipe_secret_environ();
        Ok(Self {
            ws_bind,
            proxy_bind,
            admin_bind,
            bearer,
            allowlist,
        })
    }
}

impl TunnelClientConfig {
    pub fn from_parts(
        url: String,
        bearer: String,
        allowlist: Allowlist,
        reconnect: bool,
        ws_is_loopback: bool,
    ) -> Result<Self, ConfigError> {
        ensure_token_strength("BOX_EGRESS_TUNNEL_BEARER", &bearer, ws_is_loopback)?;
        let url = url.trim().to_string();
        if !(url.starts_with("ws://") || url.starts_with("wss://")) {
            return Err(ConfigError(
                "client URL must start with ws:// or wss://".into(),
            ));
        }
        wipe_secret_environ();
        Ok(Self {
            url,
            bearer,
            allowlist,
            destination: DestinationPolicy::from_env(),
            reconnect,
        })
    }

    /// Client does **not** unlink a bearer file; the operator still needs it.
    pub fn from_env() -> Result<Self, ConfigError> {
        let url = env_nonempty("BOX_EGRESS_CLIENT_URL").ok_or_else(|| {
            ConfigError("BOX_EGRESS_CLIENT_URL must be a ws:// or wss:// URL".into())
        })?;
        let bearer = read_secret_from_env("BOX_EGRESS_TUNNEL_BEARER", false)?.ok_or_else(|| {
            ConfigError(
                "BOX_EGRESS_TUNNEL_BEARER must be set and non-empty, or BOX_EGRESS_TUNNEL_BEARER_FILE must point at a file containing it"
                    .into(),
            )
        })?;
        let allowlist =
            Allowlist::parse(&env_nonempty("BOX_EGRESS_RELAY_HOSTS").unwrap_or_default());
        let reconnect = env_bool("BOX_EGRESS_RECONNECT", false);
        let loopback = url_is_loopback(&url);
        Self::from_parts(url, bearer, allowlist, reconnect, loopback)
    }
}

/// Load a bearer from an explicit file path and/or value. File wins.
pub fn load_bearer(
    file: Option<PathBuf>,
    value: Option<String>,
    unlink_file: bool,
) -> Result<String, ConfigError> {
    if let Some(path) = file {
        std::env::set_var("BOX_EGRESS_TUNNEL_BEARER_FILE", path);
        std::env::remove_var("BOX_EGRESS_TUNNEL_BEARER");
        return read_secret_from_env("BOX_EGRESS_TUNNEL_BEARER", unlink_file)?
            .ok_or_else(|| ConfigError("BOX_EGRESS_TUNNEL_BEARER_FILE was empty".into()));
    }
    if let Some(value) = value.filter(|s| !s.trim().is_empty()) {
        return Ok(value.trim().to_string());
    }
    read_secret_from_env("BOX_EGRESS_TUNNEL_BEARER", unlink_file)?.ok_or_else(|| {
        ConfigError(
            "BOX_EGRESS_TUNNEL_BEARER must be set and non-empty, or BOX_EGRESS_TUNNEL_BEARER_FILE must point at a file containing it"
                .into(),
        )
    })
}

pub fn url_is_loopback(url: &str) -> bool {
    let rest = url
        .strip_prefix("ws://")
        .or_else(|| url.strip_prefix("wss://"))
        .unwrap_or(url);
    let hostport = rest.split('/').next().unwrap_or(rest);
    let host = hostport
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(hostport);
    let host = if host.starts_with('[') {
        host.split(']')
            .next()
            .unwrap_or(host)
            .trim_start_matches('[')
    } else {
        host.split(':').next().unwrap_or(host)
    };
    matches!(
        host,
        "127.0.0.1" | "localhost" | "::1" | "ip6-localhost" | "ip6-loopback"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_urls() {
        assert!(url_is_loopback("ws://127.0.0.1:8790"));
        assert!(url_is_loopback("ws://localhost:8790/v1/tunnel"));
        assert!(url_is_loopback("wss://[::1]:8790"));
        assert!(!url_is_loopback("ws://10.0.0.5:8790"));
        assert!(!url_is_loopback("wss://box.example.com/egress"));
    }
}
