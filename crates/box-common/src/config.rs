use std::env;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use crate::DEV_BOX_TOKEN;

/// Minimum accepted length for `BOX_TOKEN` / `BOX_HOST_TOKEN` unless
/// `BOX_ALLOW_INSECURE_DEV=1` and both daemon binds are loopback.
pub const MIN_TOKEN_LEN: usize = 16;

/// Process-wide configuration shared by `box-exec` and `box-host`.
#[derive(Clone, Debug)]
pub struct BoxConfig {
    pub box_id: String,
    pub token: String,
    pub host_token: String,
    /// voxl's password, for `box-exec` to reach `/wait` and `/record` on the box's loopback.
    /// `None` on a box without a viewer, and in `box-host`, which never needs it.
    pub viewer_password: Option<String>,
    pub workspace: PathBuf,
    pub exec_bind: SocketAddr,
    pub host_bind: SocketAddr,
    pub exec_url: String,
    pub max_file_bytes: u64,
    pub default_timeout: Duration,
    pub max_timeout: Duration,
    pub max_output_bytes: usize,
    /// Max simultaneous `POST /v1/exec` (and stream/detach) children.
    pub max_concurrent_execs: usize,
    /// Max directory listing entries returned by `GET /v1/files`.
    pub max_dir_entries: usize,
    /// After exec timeout, wait this long after SIGTERM before SIGKILL.
    pub kill_grace: Duration,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0}")]
pub struct ConfigError(pub String);

impl BoxConfig {
    /// Load config from the environment.
    ///
    /// Each bearer secret may arrive either as a value (`BOX_TOKEN`) or as a
    /// path to a file holding it (`BOX_TOKEN_FILE`). The file form is
    /// preferred and is the only one that keeps the secret out of
    /// `/proc/<pid>/environ`; see [`wipe_secret_environ`] for why the value
    /// form cannot be cleaned up after the fact.
    pub fn from_env() -> Result<Self, ConfigError> {
        let token = secret_from_env("BOX_TOKEN")?.ok_or_else(|| {
            ConfigError(
                "BOX_TOKEN must be set and non-empty, or BOX_TOKEN_FILE must point at a file containing it"
                    .into(),
            )
        })?;
        let host_token = secret_from_env("BOX_HOST_TOKEN")?.unwrap_or_else(|| token.clone());
        let viewer_password = secret_from_env("BOX_VIEWER_PASSWORD")?;
        let exec_bind = parse_addr("BOX_EXEC_BIND", "127.0.0.1:1337");
        let host_bind = parse_addr("BOX_HOST_BIND", "127.0.0.1:1340");

        validate_token("BOX_TOKEN", &token, exec_bind, host_bind)?;
        if host_token != token {
            validate_token("BOX_HOST_TOKEN", &host_token, exec_bind, host_bind)?;
        }
        if let Some(password) = &viewer_password {
            ensure_viewer_password_strength(password)?;
        }

        let workspace = PathBuf::from(env::var("WORKSPACE_ROOT").unwrap_or_else(|_| {
            if std::path::Path::new("/workspace").is_dir() {
                "/workspace".into()
            } else {
                "./workspace-data".into()
            }
        }));

        let config = Self {
            box_id: default_box_id(),
            token,
            host_token,
            viewer_password,
            workspace,
            exec_bind,
            host_bind,
            exec_url: env::var("BOX_EXEC_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:1337".to_string()),
            max_file_bytes: parse_u64("BOX_MAX_FILE_BYTES", 10 * 1024 * 1024),
            default_timeout: Duration::from_millis(parse_u64("BOX_DEFAULT_TIMEOUT_MS", 30_000)),
            max_timeout: Duration::from_millis(parse_u64("BOX_MAX_TIMEOUT_MS", 10 * 60 * 1000)),
            max_output_bytes: parse_u64("BOX_MAX_OUTPUT_BYTES", 8 * 1024 * 1024) as usize,
            max_concurrent_execs: parse_u64("BOX_MAX_CONCURRENT_EXECS", 8).clamp(1, 256) as usize,
            max_dir_entries: parse_u64("BOX_MAX_DIR_ENTRIES", 4096).clamp(1, 100_000) as usize,
            kill_grace: Duration::from_millis(
                parse_u64("BOX_EXEC_KILL_GRACE_MS", 2_000).clamp(50, 30_000),
            ),
        };

        wipe_secret_environ();
        Ok(config)
    }
}

/// Names that carry a secret *value*.
pub const SECRET_ENV_KEYS: &[&str] = &[
    "BOX_TOKEN",
    "BOX_HOST_TOKEN",
    "BOX_VIEWER_PASSWORD",
    "VOXL_PASSWORD",
    "BOX_EGRESS_TUNNEL_BEARER",
];

/// Names that carry a *path* to a secret. Not secret themselves, but a file
/// the daemon failed to unlink should not also be advertised.
pub const SECRET_FILE_ENV_KEYS: &[&str] = &[
    "BOX_TOKEN_FILE",
    "BOX_HOST_TOKEN_FILE",
    "BOX_VIEWER_PASSWORD_FILE",
    "BOX_EGRESS_TUNNEL_BEARER_FILE",
];

/// Remove the secret variables from this process's `getenv` view.
///
/// This is **not** a `/proc` defence and never was. `env::remove_var` calls
/// `unsetenv`, which rewrites the `environ` pointer array but leaves the
/// original environment block on the process stack — and that block is exactly
/// what `/proc/<pid>/environ` serves. Anything running as this uid can still
/// read a value that was passed in through the environment, for the life of
/// the process. Overwriting the block in place would be unsafe, libc-specific,
/// and racy against any other thread touching the environment, so this
/// function does not attempt it.
///
/// What it does buy: code in this process, and any library that shells out
/// without going through the exec daemon's environment filter, cannot pick the
/// secret up from `getenv`.
///
/// To keep a secret out of `/proc/<pid>/environ` it has to never enter the
/// environment in the first place. Deliver it as `BOX_TOKEN_FILE` /
/// `BOX_HOST_TOKEN_FILE` / `BOX_VIEWER_PASSWORD_FILE` /
/// `BOX_EGRESS_TUNNEL_BEARER_FILE`; the daemon reads the file and unlinks it.
/// See README §Auth for the residual exposure.
pub fn wipe_secret_environ() {
    for key in SECRET_ENV_KEYS.iter().chain(SECRET_FILE_ENV_KEYS.iter()) {
        env::remove_var(key);
    }
}

/// Read a secret delivered either as `<name>_FILE` (a path) or `<name>` (the
/// value). The file form wins when both are set.
///
/// The file is unlinked once read, so a copy staged on a writable tmpfs stops
/// being readable as soon as the daemon is up. A read-only secret mount cannot
/// be unlinked; that failure is logged rather than swallowed, because it is
/// the difference between "the secret existed for 10 ms" and "the secret is a
/// `cat` away for the life of the box".
fn secret_from_env(name: &str) -> Result<Option<String>, ConfigError> {
    read_secret_from_env(name, true)
}

/// Read `<name>_FILE` (preferred) or `<name>`.
///
/// Guest daemons pass `unlink_file = true` so a staged tmpfs copy exists only
/// until the process is up. Clients on the operator's machine pass `false` so
/// a bearer file they still need is not deleted out from under them.
pub fn read_secret_from_env(name: &str, unlink_file: bool) -> Result<Option<String>, ConfigError> {
    let file_var = format!("{name}_FILE");
    let Some(path) = crate::env_nonempty(&file_var) else {
        return Ok(crate::env_nonempty(name));
    };
    let raw = std::fs::read_to_string(&path).map_err(|err| {
        ConfigError(format!(
            "{file_var} is set to {path} but that file could not be read: {err}"
        ))
    })?;
    if unlink_file {
        if let Err(err) = std::fs::remove_file(&path) {
            tracing::warn!(
                var = %file_var,
                error = %err,
                "secret file could not be unlinked; it stays readable to anything running as this uid"
            );
        }
    }
    let value = raw.trim().to_string();
    Ok(if value.is_empty() { None } else { Some(value) })
}

fn validate_token(
    name: &str,
    token: &str,
    exec_bind: SocketAddr,
    host_bind: SocketAddr,
) -> Result<(), ConfigError> {
    ensure_token_strength(name, token, binds_are_loopback(exec_bind, host_bind))
}

/// What `.env.example` puts in every secret it ships. A value still holding it is the sample.
pub const PLACEHOLDER_MARKER: &str = "replace-with";

/// Refuse a secret still holding the sample value from `.env.example`. Nothing exempts it:
/// not `BOX_ALLOW_INSECURE_DEV`, and not a loopback bind.
pub fn ensure_not_placeholder(name: &str, value: &str) -> Result<(), ConfigError> {
    if value.to_ascii_lowercase().contains(PLACEHOLDER_MARKER) {
        return Err(ConfigError(format!(
            "{name} is still the sample value from .env.example ({PLACEHOLDER_MARKER}...). Generate one: openssl rand -base64 32"
        )));
    }
    Ok(())
}

/// Reject the `.env.example` sample always, and empty, short, or well-known tokens unless
/// `BOX_ALLOW_INSECURE_DEV=1` and every relevant bind is loopback.
pub fn ensure_token_strength(
    name: &str,
    token: &str,
    binds_are_loopback: bool,
) -> Result<(), ConfigError> {
    ensure_not_placeholder(name, token)?;
    if !token_is_insecure(token) {
        return Ok(());
    }
    if allow_insecure_dev() && binds_are_loopback {
        tracing::warn!(
            "{name} is a well-known, empty, or short value; accepted because BOX_ALLOW_INSECURE_DEV=1 and binds are loopback"
        );
        return Ok(());
    }
    Err(ConfigError(format!(
        "{name} is missing, shorter than {MIN_TOKEN_LEN} characters, or a well-known insecure value ({DEV_BOX_TOKEN}). Set a long random token, or BOX_ALLOW_INSECURE_DEV=1 with loopback binds for local demos."
    )))
}

/// The viewer password has no insecure-dev exemption. That exemption rests on the daemons binding
/// loopback, and voxl always listens on every interface inside the box, so the port map, not the
/// bind, is all that keeps it off the network.
pub fn ensure_viewer_password_strength(password: &str) -> Result<(), ConfigError> {
    ensure_not_placeholder("BOX_VIEWER_PASSWORD", password)?;
    if token_is_insecure(password) {
        return Err(ConfigError(format!(
            "BOX_VIEWER_PASSWORD is shorter than {MIN_TOKEN_LEN} characters or a well-known value. Set a long random one; BOX_ALLOW_INSECURE_DEV does not cover it."
        )));
    }
    Ok(())
}

pub fn token_is_insecure(token: &str) -> bool {
    token.is_empty() || token.len() < MIN_TOKEN_LEN || token == DEV_BOX_TOKEN
}

fn allow_insecure_dev() -> bool {
    crate::env_bool("BOX_ALLOW_INSECURE_DEV", false)
}

fn binds_are_loopback(exec_bind: SocketAddr, host_bind: SocketAddr) -> bool {
    ip_is_loopback(exec_bind.ip()) && ip_is_loopback(host_bind.ip())
}

fn ip_is_loopback(ip: IpAddr) -> bool {
    ip.is_loopback()
}

fn default_box_id() -> String {
    env::var("BOX_ID")
        .or_else(|_| env::var("HOSTNAME"))
        .or_else(|_| std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()))
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "buwiz-computer".to_string())
}

fn parse_addr(var: &str, default: &str) -> SocketAddr {
    env::var(var)
        .unwrap_or_else(|_| default.to_string())
        .parse()
        .unwrap_or_else(|_| default.parse().expect("hardcoded bind addr"))
}

fn parse_u64(var: &str, default: u64) -> u64 {
    env::var(var)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        saved: Vec<(String, Option<String>)>,
    }

    impl EnvGuard {
        fn apply(pairs: &[(&str, Option<&str>)]) -> Self {
            let saved = pairs
                .iter()
                .map(|(key, value)| {
                    let prev = env::var(key).ok();
                    match value {
                        Some(v) => env::set_var(key, v),
                        None => env::remove_var(key),
                    }
                    ((*key).to_string(), prev)
                })
                .collect();
            Self { saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, prev) in &self.saved {
                match prev {
                    Some(v) => env::set_var(key, v),
                    None => env::remove_var(key),
                }
            }
        }
    }

    fn isolate(pairs: &[(&str, Option<&str>)]) -> (std::sync::MutexGuard<'static, ()>, EnvGuard) {
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        (lock, EnvGuard::apply(pairs))
    }

    #[test]
    fn a_weak_viewer_password_is_refused() {
        let (_lock, _guard) = isolate(&[
            ("BOX_TOKEN", Some("long-enough-box-token")),
            ("BOX_TOKEN_FILE", None),
            ("BOX_HOST_TOKEN", None),
            ("BOX_HOST_TOKEN_FILE", None),
            ("BOX_VIEWER_PASSWORD", Some("short")),
            ("BOX_VIEWER_PASSWORD_FILE", None),
            ("BOX_ALLOW_INSECURE_DEV", None),
        ]);
        let err = BoxConfig::from_env().unwrap_err();
        assert!(err.0.contains("BOX_VIEWER_PASSWORD"), "{err}");
    }

    #[test]
    fn insecure_dev_does_not_excuse_a_weak_viewer_password() {
        let (_lock, _guard) = isolate(&[
            ("BOX_TOKEN", Some("short")),
            ("BOX_TOKEN_FILE", None),
            ("BOX_HOST_TOKEN", None),
            ("BOX_HOST_TOKEN_FILE", None),
            ("BOX_VIEWER_PASSWORD", Some("short")),
            ("BOX_VIEWER_PASSWORD_FILE", None),
            ("BOX_EXEC_BIND", Some("127.0.0.1:1337")),
            ("BOX_HOST_BIND", Some("127.0.0.1:1340")),
            ("BOX_ALLOW_INSECURE_DEV", Some("1")),
        ]);
        let err = BoxConfig::from_env().unwrap_err();
        assert!(err.0.contains("BOX_VIEWER_PASSWORD"), "{err}");
    }

    #[test]
    fn the_sample_token_is_refused_even_with_insecure_dev() {
        let (_lock, _guard) = isolate(&[
            ("BOX_TOKEN", Some("replace-with-a-long-random-token")),
            ("BOX_TOKEN_FILE", None),
            ("BOX_HOST_TOKEN", None),
            ("BOX_HOST_TOKEN_FILE", None),
            ("BOX_VIEWER_PASSWORD", None),
            ("BOX_VIEWER_PASSWORD_FILE", None),
            ("BOX_EXEC_BIND", Some("127.0.0.1:1337")),
            ("BOX_HOST_BIND", Some("127.0.0.1:1340")),
            ("BOX_ALLOW_INSECURE_DEV", Some("1")),
        ]);
        let err = BoxConfig::from_env().unwrap_err();
        assert!(
            err.0.contains("BOX_TOKEN is still the sample value"),
            "{err}"
        );
        assert!(err.0.contains("openssl rand"), "{err}");
    }

    #[test]
    fn the_sample_host_token_is_refused() {
        let (_lock, _guard) = isolate(&[
            ("BOX_TOKEN", Some("long-enough-box-token")),
            ("BOX_TOKEN_FILE", None),
            ("BOX_HOST_TOKEN", Some("Replace-With-a-host-token")),
            ("BOX_HOST_TOKEN_FILE", None),
            ("BOX_VIEWER_PASSWORD", None),
            ("BOX_VIEWER_PASSWORD_FILE", None),
            ("BOX_ALLOW_INSECURE_DEV", None),
        ]);
        let err = BoxConfig::from_env().unwrap_err();
        assert!(
            err.0.contains("BOX_HOST_TOKEN is still the sample"),
            "{err}"
        );
    }

    #[test]
    fn the_sample_viewer_password_is_refused() {
        let (_lock, _guard) = isolate(&[
            ("BOX_TOKEN", Some("long-enough-box-token")),
            ("BOX_TOKEN_FILE", None),
            ("BOX_HOST_TOKEN", None),
            ("BOX_HOST_TOKEN_FILE", None),
            (
                "BOX_VIEWER_PASSWORD",
                Some("replace-with-a-long-random-password"),
            ),
            ("BOX_VIEWER_PASSWORD_FILE", None),
            ("BOX_ALLOW_INSECURE_DEV", Some("1")),
        ]);
        let err = BoxConfig::from_env().unwrap_err();
        assert!(
            err.0.contains("BOX_VIEWER_PASSWORD is still the sample"),
            "{err}"
        );
    }

    #[test]
    fn a_placeholder_is_refused_wherever_a_token_is_checked() {
        let err = ensure_token_strength("X", "replace-with-something-long", true).unwrap_err();
        assert!(err.0.contains("X is still the sample"), "{err}");
        assert!(ensure_not_placeholder("X", "a-real-random-token-value").is_ok());
    }

    #[test]
    fn default_box_id_never_empty() {
        assert!(!default_box_id().is_empty());
    }

    #[test]
    fn token_is_insecure_flags_known_bad_values() {
        assert!(token_is_insecure(""));
        assert!(token_is_insecure("short"));
        assert!(token_is_insecure(DEV_BOX_TOKEN));
        assert!(!token_is_insecure("local-smoke-box-token"));
    }

    #[test]
    fn from_env_requires_token() {
        let (_lock, _guard) = isolate(&[
            ("BOX_TOKEN", None),
            ("BOX_HOST_TOKEN", None),
            ("BOX_VIEWER_PASSWORD", None),
            ("BOX_ALLOW_INSECURE_DEV", None),
            ("BOX_EXEC_BIND", Some("127.0.0.1:1337")),
            ("BOX_HOST_BIND", Some("127.0.0.1:1340")),
        ]);
        let err = BoxConfig::from_env().unwrap_err();
        assert!(err.0.contains("BOX_TOKEN must be set"), "{err}");
    }

    #[test]
    fn from_env_rejects_dev_token_without_opt_in() {
        let (_lock, _guard) = isolate(&[
            ("BOX_TOKEN", Some(DEV_BOX_TOKEN)),
            ("BOX_HOST_TOKEN", None),
            ("BOX_VIEWER_PASSWORD", Some("viewer-password-one")),
            ("BOX_ALLOW_INSECURE_DEV", None),
            ("BOX_EXEC_BIND", Some("127.0.0.1:1337")),
            ("BOX_HOST_BIND", Some("127.0.0.1:1340")),
        ]);
        let err = BoxConfig::from_env().unwrap_err();
        assert!(err.0.contains("insecure"), "{err}");
    }

    #[test]
    fn from_env_rejects_insecure_token_on_unspecified_bind_even_with_opt_in() {
        let (_lock, _guard) = isolate(&[
            ("BOX_TOKEN", Some(DEV_BOX_TOKEN)),
            ("BOX_HOST_TOKEN", None),
            ("BOX_VIEWER_PASSWORD", Some("viewer-password-one")),
            ("BOX_ALLOW_INSECURE_DEV", Some("1")),
            ("BOX_EXEC_BIND", Some("0.0.0.0:1337")),
            ("BOX_HOST_BIND", Some("0.0.0.0:1340")),
        ]);
        let err = BoxConfig::from_env().unwrap_err();
        assert!(err.0.contains("loopback"), "{err}");
    }

    #[test]
    fn from_env_allows_insecure_token_on_loopback_with_opt_in() {
        let (_lock, _guard) = isolate(&[
            ("BOX_TOKEN", Some(DEV_BOX_TOKEN)),
            ("BOX_HOST_TOKEN", None),
            ("BOX_VIEWER_PASSWORD", Some("viewer-password-one")),
            ("BOX_ALLOW_INSECURE_DEV", Some("1")),
            ("BOX_EXEC_BIND", Some("127.0.0.1:1337")),
            ("BOX_HOST_BIND", Some("127.0.0.1:1340")),
            ("WORKSPACE_ROOT", Some("/tmp")),
        ]);
        let config = BoxConfig::from_env().unwrap();
        assert_eq!(config.token, DEV_BOX_TOKEN);
        assert!(env::var("BOX_TOKEN").is_err());
        assert!(env::var("BOX_VIEWER_PASSWORD").is_err());
    }

    #[test]
    fn from_env_accepts_long_token_and_wipes_environ() {
        let (_lock, _guard) = isolate(&[
            ("BOX_TOKEN", Some("local-smoke-box-token")),
            ("BOX_HOST_TOKEN", Some("local-smoke-host-token")),
            ("BOX_VIEWER_PASSWORD", Some("viewer-password-one")),
            ("BOX_ALLOW_INSECURE_DEV", None),
            ("BOX_EXEC_BIND", Some("0.0.0.0:1337")),
            ("BOX_HOST_BIND", Some("0.0.0.0:1340")),
            ("WORKSPACE_ROOT", Some("/tmp")),
        ]);
        let config = BoxConfig::from_env().unwrap();
        assert_eq!(config.token, "local-smoke-box-token");
        assert_eq!(config.host_token, "local-smoke-host-token");
        assert_eq!(config.exec_bind, "0.0.0.0:1337".parse().unwrap());
        assert!(env::var("BOX_TOKEN").is_err());
        assert!(env::var("BOX_HOST_TOKEN").is_err());
        assert!(env::var("BOX_VIEWER_PASSWORD").is_err());
    }

    #[test]
    fn token_file_is_preferred_over_the_value_and_is_unlinked() {
        let dir = tempfile::TempDir::new().unwrap();
        let token_path = dir.path().join("box_token");
        std::fs::write(&token_path, "file-delivered-box-token\n").unwrap();
        let (_lock, _guard) = isolate(&[
            ("BOX_TOKEN", Some("env-delivered-box-token")),
            ("BOX_TOKEN_FILE", Some(token_path.to_str().unwrap())),
            ("BOX_HOST_TOKEN", None),
            ("BOX_HOST_TOKEN_FILE", None),
            ("BOX_VIEWER_PASSWORD", None),
            ("BOX_ALLOW_INSECURE_DEV", None),
            ("BOX_EXEC_BIND", Some("0.0.0.0:1337")),
            ("BOX_HOST_BIND", Some("0.0.0.0:1340")),
            ("WORKSPACE_ROOT", Some("/tmp")),
        ]);
        let config = BoxConfig::from_env().unwrap();
        assert_eq!(config.token, "file-delivered-box-token");
        assert_eq!(config.host_token, "file-delivered-box-token");
        assert!(
            !token_path.exists(),
            "the daemon must unlink a secret file it can write to"
        );
        assert!(env::var("BOX_TOKEN_FILE").is_err());
    }

    #[test]
    fn host_token_file_can_differ_from_the_box_token_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let token_path = dir.path().join("box_token");
        let host_path = dir.path().join("host_token");
        std::fs::write(&token_path, "file-delivered-box-token").unwrap();
        std::fs::write(&host_path, "file-delivered-host-token").unwrap();
        let (_lock, _guard) = isolate(&[
            ("BOX_TOKEN", None),
            ("BOX_TOKEN_FILE", Some(token_path.to_str().unwrap())),
            ("BOX_HOST_TOKEN", None),
            ("BOX_HOST_TOKEN_FILE", Some(host_path.to_str().unwrap())),
            ("BOX_VIEWER_PASSWORD", None),
            ("BOX_ALLOW_INSECURE_DEV", None),
            ("BOX_EXEC_BIND", Some("0.0.0.0:1337")),
            ("BOX_HOST_BIND", Some("0.0.0.0:1340")),
            ("WORKSPACE_ROOT", Some("/tmp")),
        ]);
        let config = BoxConfig::from_env().unwrap();
        assert_eq!(config.token, "file-delivered-box-token");
        assert_eq!(config.host_token, "file-delivered-host-token");
    }

    #[test]
    fn unreadable_token_file_fails_instead_of_falling_back_to_the_env_value() {
        let (_lock, _guard) = isolate(&[
            ("BOX_TOKEN", Some("env-delivered-box-token")),
            ("BOX_TOKEN_FILE", Some("/nonexistent/box_token")),
            ("BOX_HOST_TOKEN", None),
            ("BOX_HOST_TOKEN_FILE", None),
            ("BOX_VIEWER_PASSWORD", None),
            ("BOX_ALLOW_INSECURE_DEV", None),
            ("BOX_EXEC_BIND", Some("127.0.0.1:1337")),
            ("BOX_HOST_BIND", Some("127.0.0.1:1340")),
        ]);
        let err = BoxConfig::from_env().unwrap_err();
        assert!(err.0.contains("BOX_TOKEN_FILE"), "{err}");
    }

    #[test]
    fn rust_default_bind_is_loopback() {
        let addr: SocketAddr = parse_addr("BOX_EXEC_BIND_MISSING_FOR_TEST", "127.0.0.1:1337");
        assert!(addr.ip().is_loopback());
    }
}
