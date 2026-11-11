//! Config + probe used by `box-host` (`capabilities.egress_tunnel`, `capabilities.egress_proxy`,
//! `GET /v1/egress`).
//!
//! `ready` means a laptop client is **currently attached**. Enabled but no
//! client is `enabled: true, ready: false`. The CONNECT proxy fail-closes in
//! that state when the rules say the tunnel is the exit.
//!
//! Nothing in a status names a site: the rules are counts, and the counts are outcomes.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use box_common::{
    container_local_host, env_bool, env_nonempty, local_get, parse_addr, ConfigError,
};
use box_egress::rules::{Exit, RulesSummary};
use serde::{Deserialize, Serialize};

use crate::config::DEFAULT_RULES_PATH;
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
    /// `BOX_EGRESS_TUNNEL`: the box was made with the tunnel.
    pub enabled: bool,
    /// `BOX_EGRESS_PROXY`, or the tunnel: the browser goes through the proxy, under the rules.
    pub proxy: bool,
    pub ws_bind: SocketAddr,
    pub proxy_bind: SocketAddr,
    pub admin_bind: SocketAddr,
    pub rules_path: PathBuf,
}

impl EgressConfig {
    /// A malformed `BOX_EGRESS_*_BIND` is an error naming the variable; unset or empty is the
    /// default.
    pub fn from_env() -> Result<Self, ConfigError> {
        let enabled = env_bool("BOX_EGRESS_TUNNEL", false);
        Ok(Self {
            enabled,
            proxy: enabled || env_bool("BOX_EGRESS_PROXY", false),
            ws_bind: parse_addr("BOX_EGRESS_WS_BIND", DEFAULT_WS)?,
            proxy_bind: parse_addr("BOX_EGRESS_PROXY_BIND", DEFAULT_PROXY)?,
            admin_bind: parse_addr("BOX_EGRESS_ADMIN_BIND", DEFAULT_ADMIN)?,
            rules_path: env_nonempty("BOX_EGRESS_RULES_PATH")
                .unwrap_or_else(|| DEFAULT_RULES_PATH.into())
                .into(),
        })
    }

    pub fn disabled() -> Self {
        Self {
            enabled: false,
            proxy: false,
            ws_bind: DEFAULT_WS.parse().expect("default ws"),
            proxy_bind: DEFAULT_PROXY.parse().expect("default proxy"),
            admin_bind: DEFAULT_ADMIN.parse().expect("default admin"),
            rules_path: DEFAULT_RULES_PATH.into(),
        }
    }
}

/// What the proxy did, since it started: outcomes only, never a site.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Counts {
    /// Dialled by the box itself.
    pub direct: u64,
    /// Relayed through the operator's client.
    pub tunneled: u64,
    /// On the block list.
    pub blocked: u64,
    /// Not on the allow list, in `listed` mode.
    pub not_listed: u64,
    /// Refused because no rules were set.
    pub no_rules: u64,
    /// The tunnel was the exit and no client was attached.
    pub no_client: u64,
    /// Every stream slot in use.
    pub streams_full: u64,
    /// The address the name resolved to is private, loopback, reserved, or on a port not relayed.
    pub destination: u64,
    /// The name didn't resolve, the dial failed or timed out, or the client couldn't open it.
    pub failed: u64,
    /// Plain `http://` (absolute-form) requests, which are refused.
    pub plain_http: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EgressStatus {
    /// The box was made with the tunnel (`BOX_EGRESS_TUNNEL`).
    pub enabled: bool,
    /// A client is attached and CONNECT will be relayed.
    pub ready: bool,
    pub client_attached: bool,
    pub protocol: String,
    /// Container-local listen inventory (unspecified rewritten to loopback).
    pub ws: String,
    pub proxy: String,
    /// The browser goes through the proxy (`BOX_EGRESS_PROXY`, or the tunnel).
    #[serde(default)]
    pub proxy_enabled: bool,
    /// The proxy answers.
    #[serde(default)]
    pub proxy_up: bool,
    /// The rules in force, as counts. `null`: none, and every site is refused.
    #[serde(default)]
    pub rules: Option<RulesSummary>,
    /// Why the last rules file read failed, if it did. Names no site.
    #[serde(default)]
    pub rules_error: Option<String>,
    /// A page can load: rules are set, and their exit is direct or has a client attached.
    #[serde(default)]
    pub can_browse: bool,
    #[serde(default)]
    pub counts: Counts,
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
            proxy_enabled: cfg.proxy,
            proxy_up: false,
            rules: None,
            rules_error: None,
            can_browse: false,
            counts: Counts::default(),
        }
    }

    /// Whether a page can load, by these rules and this attach state.
    pub fn browsable(rules: Option<&RulesSummary>, attached: bool) -> bool {
        match rules {
            Some(rules) => rules.exit == Exit::Direct || attached,
            None => false,
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

/// Honest probe: with neither the proxy nor the tunnel, do not touch the admin port. Otherwise
/// `GET /v1/status` on the admin loopback. Process down → enabled, not ready, proxy not up.
pub async fn probe_egress(cfg: &EgressConfig) -> EgressStatus {
    if !cfg.proxy {
        return EgressStatus::from_config(cfg, false);
    }
    match fetch_admin(cfg.admin_bind).await {
        Some(mut st) => {
            st.enabled = cfg.enabled;
            st.proxy_enabled = true;
            st.proxy_up = true;
            st.ready = st.client_attached;
            st.can_browse = EgressStatus::browsable(st.rules.as_ref(), st.client_attached);
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
        assert!(!st.proxy_enabled);
        assert!(!st.can_browse);
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
        assert_eq!(cfg.rules_path, PathBuf::from(DEFAULT_RULES_PATH));
    }

    #[tokio::test]
    async fn closed_admin_is_enabled_not_ready() {
        let cfg = EgressConfig {
            enabled: true,
            proxy: true,
            ws_bind: "127.0.0.1:8790".parse().unwrap(),
            proxy_bind: "127.0.0.1:8791".parse().unwrap(),
            admin_bind: "127.0.0.1:1".parse().unwrap(),
            rules_path: DEFAULT_RULES_PATH.into(),
        };
        let st = probe_egress(&cfg).await;
        assert!(st.enabled);
        assert!(!st.ready);
        assert!(!st.proxy_up);
        assert!(!st.can_browse);
        assert_eq!(st.proxy, "127.0.0.1:8791");
    }

    #[test]
    fn a_page_loads_with_rules_and_a_reachable_exit_only() {
        let direct = RulesSummary {
            version: 1,
            exit: Exit::Direct,
            mode: box_egress::Mode::All,
            allow: 0,
            block: 0,
        };
        let tunnel = RulesSummary {
            exit: Exit::Tunnel,
            ..direct.clone()
        };
        assert!(!EgressStatus::browsable(None, true));
        assert!(EgressStatus::browsable(Some(&direct), false));
        assert!(!EgressStatus::browsable(Some(&tunnel), false));
        assert!(EgressStatus::browsable(Some(&tunnel), true));
    }
}
