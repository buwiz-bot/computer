//! Bind addresses, bearer, rules and allowlist for the tunnel process.
//!
//! Everything here runs before the async runtime starts (`main.rs`), so reading and wiping the
//! environment never races another thread.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use box_common::{ensure_token_strength, env_nonempty, ConfigError};
use box_egress::client::ClientState;
use box_egress::rules::{Rules, RulesCell};
use box_egress::TunnelClientConfig;

use crate::allowlist::Allowlist;
use crate::destination::DestinationPolicy;

/// Where box-host writes the rules and the server reads them, unless `BOX_EGRESS_RULES_PATH`
/// says otherwise. Its folder belongs to box-host's user: nothing else in the box can write it.
pub const DEFAULT_RULES_PATH: &str = "/run/box/egress-rules/rules.json";

/// The tunnel's WebSocket side: only when the box was made with the tunnel.
#[derive(Clone, Debug)]
pub struct WsSide {
    pub bind: SocketAddr,
    pub bearer: String,
}

#[derive(Clone, Debug)]
pub struct TunnelServerConfig {
    /// `None`: a proxy with no tunnel, which only ever dials directly.
    pub tunnel: Option<WsSide>,
    pub proxy_bind: SocketAddr,
    pub admin_bind: SocketAddr,
    /// The operator's static name filter (`BOX_EGRESS_RELAY_HOSTS`), on top of the rules.
    pub allowlist: Allowlist,
    /// What the proxy may dial when it dials directly.
    pub destination: DestinationPolicy,
    /// The rules file, read at start and on each reload. `None`: only `rules` below, as set.
    pub rules_path: Option<PathBuf>,
    /// The rules in force. Empty refuses every site.
    pub rules: RulesCell,
}

impl TunnelServerConfig {
    /// The proxy and the admin port are for this box alone. The proxy dials out for whoever
    /// reaches it, so a proxy on another interface is an open relay: refused, not warned about.
    pub fn check_binds(&self) -> Result<(), ConfigError> {
        if !self.proxy_bind.ip().is_loopback() {
            return Err(ConfigError(format!(
                "BOX_EGRESS_PROXY_BIND must be a loopback address (it is {}): the proxy dials out for whoever reaches it",
                self.proxy_bind
            )));
        }
        if !self.admin_bind.ip().is_loopback() {
            return Err(ConfigError(format!(
                "BOX_EGRESS_ADMIN_BIND must be a loopback address (it is {}): its status and reload routes have no login",
                self.admin_bind
            )));
        }
        Ok(())
    }
}

/// Read the rules file. Missing is no rules (`Ok(None)`); unreadable or malformed is an error,
/// which the server also treats as no rules.
pub fn read_rules(path: &Path) -> Result<Option<Rules>, String> {
    match std::fs::read(path) {
        Ok(bytes) => Rules::parse(&bytes)
            .map(Some)
            .map_err(|err| err.to_string()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("the rules file could not be read: {}", err.kind())),
    }
}

/// The bearer, from a file (preferred) or `BOX_EGRESS_TUNNEL_BEARER`. `unlink` removes the file
/// once read: for the copy box-keeper staged, never for an operator's own file. Call before the
/// runtime starts, then [`box_common::wipe_secret_environ`].
pub fn load_bearer(file: Option<&Path>, unlink: bool) -> Result<String, ConfigError> {
    if let Some(path) = file {
        let raw = std::fs::read_to_string(path).map_err(|err| {
            ConfigError(format!(
                "the bearer file {} could not be read: {err}",
                path.display()
            ))
        })?;
        if unlink {
            if let Err(err) = std::fs::remove_file(path) {
                eprintln!(
                    "the staged bearer file could not be unlinked ({err}); it stays readable to anything running as this uid"
                );
            }
        }
        let value = raw.trim().to_string();
        if value.is_empty() {
            return Err(ConfigError("the bearer file was empty".into()));
        }
        return Ok(value);
    }
    env_nonempty("BOX_EGRESS_TUNNEL_BEARER")
        .map(|value| value.trim().to_string())
        .ok_or_else(|| {
            ConfigError(
                "a bearer is needed: --bearer-file, --staged-bearer-file, BOX_EGRESS_TUNNEL_BEARER_FILE or BOX_EGRESS_TUNNEL_BEARER"
                    .into(),
            )
        })
}

/// A client config, its bearer checked like the box's own token.
pub fn client_config(
    url: String,
    bearer: String,
    allowlist: Allowlist,
    reconnect: bool,
) -> Result<TunnelClientConfig, ConfigError> {
    let url = url.trim().to_string();
    if !(url.starts_with("ws://") || url.starts_with("wss://")) {
        return Err(ConfigError(
            "client URL must start with ws:// or wss://".into(),
        ));
    }
    ensure_token_strength("BOX_EGRESS_TUNNEL_BEARER", &bearer, url_is_loopback(&url))?;
    Ok(TunnelClientConfig {
        url,
        bearer,
        allowlist,
        destination: DestinationPolicy::from_env(),
        // An operator's own client has no computer behind it to set rules: every site, as before,
        // with the address policy and `BOX_EGRESS_RELAY_HOSTS` still applied.
        rules: RulesCell::new(Some(open_rules())),
        reconnect,
        state: ClientState::default(),
    })
}

/// Every site, through the tunnel: what a box with the tunnel and no rules file has always done.
pub fn open_rules() -> Rules {
    Rules::new(0, box_egress::Exit::Tunnel, box_egress::Mode::All, &[], &[])
        .expect("no entries to refuse")
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

    fn server(proxy: &str, admin: &str) -> TunnelServerConfig {
        TunnelServerConfig {
            tunnel: None,
            proxy_bind: proxy.parse().unwrap(),
            admin_bind: admin.parse().unwrap(),
            allowlist: Allowlist::any(),
            destination: DestinationPolicy::default(),
            rules_path: None,
            rules: RulesCell::default(),
        }
    }

    #[test]
    fn a_proxy_or_admin_port_off_loopback_is_refused() {
        assert!(server("127.0.0.1:8791", "127.0.0.1:8792")
            .check_binds()
            .is_ok());
        let err = server("0.0.0.0:8791", "127.0.0.1:8792")
            .check_binds()
            .unwrap_err();
        assert!(err.0.contains("BOX_EGRESS_PROXY_BIND"), "{err}");
        let err = server("127.0.0.1:8791", "0.0.0.0:8792")
            .check_binds()
            .unwrap_err();
        assert!(err.0.contains("BOX_EGRESS_ADMIN_BIND"), "{err}");
    }

    #[test]
    fn a_staged_bearer_file_is_unlinked_and_an_operators_own_is_kept() {
        let dir = std::env::temp_dir().join(format!("egress-bearer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let own = dir.join("own");
        std::fs::write(&own, "  own-bearer-value-of-some-length  \n").unwrap();
        assert_eq!(
            load_bearer(Some(&own), false).unwrap(),
            "own-bearer-value-of-some-length"
        );
        assert!(own.exists(), "an operator's own bearer file stays");
        let staged = dir.join("staged");
        std::fs::write(&staged, "staged-bearer-value").unwrap();
        assert_eq!(
            load_bearer(Some(&staged), true).unwrap(),
            "staged-bearer-value"
        );
        assert!(!staged.exists(), "the staged copy goes once read");
        let empty = dir.join("empty");
        std::fs::write(&empty, "\n").unwrap();
        assert!(load_bearer(Some(&empty), false).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_rules_file_that_is_missing_or_bad_is_no_rules() {
        let dir = std::env::temp_dir().join(format!("egress-rules-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rules.json");
        assert_eq!(read_rules(&path), Ok(None));
        std::fs::write(
            &path,
            br#"{"v":1,"exit":"direct","mode":"all","allow":["*"]}"#,
        )
        .unwrap();
        assert!(read_rules(&path).is_err());
        std::fs::write(&path, br#"{"v":1,"exit":"direct","mode":"all"}"#).unwrap();
        assert!(read_rules(&path).unwrap().is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
