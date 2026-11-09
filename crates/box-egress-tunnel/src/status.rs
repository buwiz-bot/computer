//! Config + probe used by `box-host` (`capabilities.egress_tunnel`, `GET /v1/egress`).
//!
//! `ready` means a laptop client is **currently attached**. Enabled but no
//! client is `enabled: true, ready: false`. The CONNECT proxy fail-closes in
//! that state.

use std::net::SocketAddr;
use std::time::Duration;

use box_common::{container_local_host, env_bool, local_get, parse_addr, ConfigError};
use serde::{Deserialize, Serialize};

use crate::protocol::PROTOCOL_NAME;

const DEFAULT_WS: &str = "127.0.0.1:8790";
const DEFAULT_PROXY: &str = "127.0.0.1:8791";
const DEFAULT_ADMIN: &str = "127.0.0.1:8792";
/// Longest the admin status probe may take, connect to last byte.
const ADMIN_DEADLINE: Duration = Duration::from_millis(600);
/// A status answer is a few hundred bytes.
const ADMIN_CAP: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EgressConfig {
    pub enabled: bool,
    pub ws_bind: SocketAddr,
    pub proxy_bind: SocketAddr,
    pub admin_bind: SocketAddr,
}

impl EgressConfig {
    /// A malformed `BOX_EGRESS_*_BIND` is an error naming the variable; unset or empty is the
    /// default.
    pub fn from_env() -> Result<Self, ConfigError> {
        Ok(Self {
            enabled: env_bool("BOX_EGRESS_TUNNEL", false),
            ws_bind: parse_addr("BOX_EGRESS_WS_BIND", DEFAULT_WS)?,
            proxy_bind: parse_addr("BOX_EGRESS_PROXY_BIND", DEFAULT_PROXY)?,
            admin_bind: parse_addr("BOX_EGRESS_ADMIN_BIND", DEFAULT_ADMIN)?,
        })
    }

    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ws_bind: DEFAULT_WS.parse().expect("default ws"),
            proxy_bind: DEFAULT_PROXY.parse().expect("default proxy"),
            admin_bind: DEFAULT_ADMIN.parse().expect("default admin"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EgressStatus {
    pub enabled: bool,
    /// A client is attached and CONNECT will be relayed.
    pub ready: bool,
    pub client_attached: bool,
    pub protocol: String,
    /// Container-local listen inventory (unspecified rewritten to loopback).
    pub ws: String,
    pub proxy: String,
}

impl EgressStatus {
    pub fn from_config(cfg: &EgressConfig, ready: bool) -> Self {
        Self {
            enabled: cfg.enabled,
            ready,
            client_attached: ready,
            protocol: PROTOCOL_NAME.to_string(),
            ws: format_bind(cfg.ws_bind),
            proxy: format_bind(cfg.proxy_bind),
        }
    }
}

pub fn format_bind(bind: SocketAddr) -> String {
    let host = container_local_host(bind);
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{}", bind.port())
    } else {
        format!("{host}:{}", bind.port())
    }
}

/// Honest probe: when disabled, do not touch the admin port. When enabled,
/// `GET /v1/status` on the admin loopback. Process down → enabled, not ready.
pub async fn probe_egress(cfg: &EgressConfig) -> EgressStatus {
    if !cfg.enabled {
        return EgressStatus::from_config(cfg, false);
    }
    match fetch_admin(cfg.admin_bind).await {
        Some(mut st) => {
            st.enabled = true;
            st.ready = st.client_attached;
            if st.protocol.is_empty() {
                st.protocol = PROTOCOL_NAME.to_string();
            }
            st
        }
        None => EgressStatus::from_config(cfg, false),
    }
}

async fn fetch_admin(addr: SocketAddr) -> Option<EgressStatus> {
    let answer = local_get(&addr.to_string(), "/v1/status", ADMIN_DEADLINE, ADMIN_CAP)
        .await
        .ok()?;
    if answer.status != 200 {
        return None;
    }
    serde_json::from_slice(&answer.body).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn disabled_is_not_ready() {
        let st = probe_egress(&EgressConfig::disabled()).await;
        assert!(!st.enabled);
        assert!(!st.ready);
        assert!(!st.client_attached);
        assert_eq!(st.protocol, PROTOCOL_NAME);
    }

    // The only test in this crate that touches these variables, so it needs no lock.
    #[test]
    fn a_malformed_egress_bind_is_an_error_and_unset_or_empty_is_the_default() {
        std::env::remove_var("BOX_EGRESS_WS_BIND");
        std::env::remove_var("BOX_EGRESS_ADMIN_BIND");
        std::env::set_var("BOX_EGRESS_PROXY_BIND", "0.0.0.0:879x");
        let err = EgressConfig::from_env().unwrap_err();
        assert!(err.0.contains("BOX_EGRESS_PROXY_BIND"), "{err}");
        assert!(err.0.contains("0.0.0.0:879x"), "{err}");

        std::env::set_var("BOX_EGRESS_PROXY_BIND", " ");
        let cfg = EgressConfig::from_env().unwrap();
        assert_eq!(cfg.proxy_bind, DEFAULT_PROXY.parse().unwrap());

        std::env::remove_var("BOX_EGRESS_PROXY_BIND");
        let cfg = EgressConfig::from_env().unwrap();
        assert_eq!(cfg.proxy_bind, DEFAULT_PROXY.parse().unwrap());
        assert_eq!(cfg.ws_bind, DEFAULT_WS.parse().unwrap());
        assert_eq!(cfg.admin_bind, DEFAULT_ADMIN.parse().unwrap());

        // The tunnel daemon reads the same variables the same way, before its bearer.
        std::env::set_var("BOX_EGRESS_WS_BIND", "nowhere");
        let err = crate::config::TunnelServerConfig::from_env().unwrap_err();
        assert!(err.0.contains("BOX_EGRESS_WS_BIND"), "{err}");
        std::env::remove_var("BOX_EGRESS_WS_BIND");
    }

    #[tokio::test]
    async fn closed_admin_is_enabled_not_ready() {
        let cfg = EgressConfig {
            enabled: true,
            ws_bind: "127.0.0.1:8790".parse().unwrap(),
            proxy_bind: "127.0.0.1:8791".parse().unwrap(),
            admin_bind: "127.0.0.1:1".parse().unwrap(),
        };
        let st = probe_egress(&cfg).await;
        assert!(st.enabled);
        assert!(!st.ready);
        assert_eq!(st.proxy, "127.0.0.1:8791");
    }
}
