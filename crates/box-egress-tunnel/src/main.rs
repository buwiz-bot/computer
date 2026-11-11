use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use box_common::ensure_token_strength;
use box_egress::destination::DestinationPolicy;
use box_egress::rules::RulesCell;
use box_egress_tunnel::config::{client_config, load_bearer};
use box_egress_tunnel::{Allowlist, TunnelServerConfig, WsSide, PROTOCOL_NAME};
use clap::{Parser, Subcommand};

// No flag takes a bearer's value: argv is in `ps` and in every `--help` that shows an env value.
// A bearer comes in a file, or in BOX_EGRESS_TUNNEL_BEARER, read here before any thread starts and
// wiped from this process's environment.
#[derive(Parser)]
#[command(
    name = "box-egress-tunnel",
    version,
    about = "box-egress-v1: the guest's CONNECT proxy, under the computer's rules, with an optional tunnel to a client on the operator's machine"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Listen in the guest: the CONNECT proxy and the loopback admin port, and with --tunnel the
    /// WebSocket for the operator's client.
    Server {
        #[arg(long, env = "BOX_EGRESS_PROXY_BIND", default_value = "127.0.0.1:8791")]
        proxy_bind: SocketAddr,
        #[arg(long, env = "BOX_EGRESS_ADMIN_BIND", default_value = "127.0.0.1:8792")]
        admin_bind: SocketAddr,
        /// Open the tunnel's WebSocket. Needs a bearer.
        #[arg(long)]
        tunnel: bool,
        #[arg(long, env = "BOX_EGRESS_WS_BIND", default_value = "127.0.0.1:8790")]
        ws_bind: SocketAddr,
        /// A bearer file to read and keep.
        #[arg(long, env = "BOX_EGRESS_TUNNEL_BEARER_FILE", hide_env_values = true)]
        bearer_file: Option<PathBuf>,
        /// A staged copy of the bearer (box-keeper's), unlinked once read.
        #[arg(long, conflicts_with = "bearer_file")]
        staged_bearer_file: Option<PathBuf>,
        /// The rules file: read at start and on each `POST /v1/rules/reload` to the admin port.
        /// Missing or malformed is no rules, which refuses every site.
        #[arg(
            long,
            env = "BOX_EGRESS_RULES_PATH",
            default_value = box_egress_tunnel::config::DEFAULT_RULES_PATH
        )]
        rules_path: PathBuf,
        /// Comma-separated CONNECT hosts (`*.example.com` ok), on top of the rules. Empty = no
        /// extra filter.
        #[arg(long, env = "BOX_EGRESS_RELAY_HOSTS")]
        relay_hosts: Option<String>,
    },
    /// Attach from the operator's machine and relay outbound TCP.
    Client {
        /// `ws://` or `wss://` URL of the guest tunnel (published 8790, or SSH -L).
        #[arg(long, env = "BOX_EGRESS_CLIENT_URL")]
        url: String,
        /// A bearer file to read and keep.
        #[arg(long, env = "BOX_EGRESS_TUNNEL_BEARER_FILE", hide_env_values = true)]
        bearer_file: Option<PathBuf>,
        #[arg(long, env = "BOX_EGRESS_RELAY_HOSTS")]
        relay_hosts: Option<String>,
        /// Repeatable extra host allowlist entries.
        #[arg(long = "allow-host")]
        allow_host: Vec<String>,
        /// Reconnect on drop (not on 401).
        #[arg(long, env = "BOX_EGRESS_RECONNECT")]
        reconnect: bool,
    },
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .compact()
        .init();
}

enum Job {
    Server(TunnelServerConfig),
    Client(box_egress::TunnelClientConfig),
}

/// Everything that reads the environment, before any thread exists.
fn prepare() -> Result<Job> {
    let cli = Cli::parse();
    let job = match cli.command {
        Command::Server {
            proxy_bind,
            admin_bind,
            tunnel,
            ws_bind,
            bearer_file,
            staged_bearer_file,
            rules_path,
            relay_hosts,
        } => {
            let tunnel = if tunnel {
                let bearer = match (&staged_bearer_file, &bearer_file) {
                    (Some(staged), _) => load_bearer(Some(staged), true)?,
                    (None, file) => load_bearer(file.as_deref(), false)?,
                };
                ensure_token_strength(
                    "BOX_EGRESS_TUNNEL_BEARER",
                    &bearer,
                    ws_bind.ip().is_loopback(),
                )?;
                Some(WsSide {
                    bind: ws_bind,
                    bearer,
                })
            } else {
                None
            };
            let cfg = TunnelServerConfig {
                tunnel,
                proxy_bind,
                admin_bind,
                allowlist: merge_allowlist(relay_hosts, Vec::new())?,
                destination: DestinationPolicy::from_env().map_err(anyhow::Error::msg)?,
                rules_path: Some(rules_path),
                rules: RulesCell::default(),
            };
            cfg.check_binds()?;
            Job::Server(cfg)
        }
        Command::Client {
            url,
            bearer_file,
            relay_hosts,
            allow_host,
            reconnect,
        } => {
            let bearer = load_bearer(bearer_file.as_deref(), false)?;
            let allowlist = merge_allowlist(relay_hosts, allow_host)?;
            Job::Client(client_config(url, bearer, allowlist, reconnect)?)
        }
    };
    // Children inherit neither the values nor where they came from.
    box_common::wipe_secret_environ();
    Ok(job)
}

fn main() {
    box_common::make_undumpable();
    init_tracing();
    let job = match prepare() {
        Ok(job) => job,
        Err(err) => {
            tracing::error!(error = %err, "box-egress-tunnel exited");
            std::process::exit(1);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            tracing::error!(error = %err, "box-egress-tunnel exited");
            std::process::exit(1);
        }
    };
    if let Err(err) = runtime.block_on(run(job)) {
        tracing::error!(error = %err, "box-egress-tunnel exited");
        std::process::exit(1);
    }
}

async fn run(job: Job) -> Result<()> {
    match job {
        Job::Server(cfg) => {
            tracing::info!(
                protocol = PROTOCOL_NAME,
                tunnel = cfg.tunnel.is_some(),
                proxy_bind = %cfg.proxy_bind,
                admin_bind = %cfg.admin_bind,
                "starting server"
            );
            box_egress_tunnel::server::run(cfg).await.context("server")
        }
        Job::Client(cfg) => box_egress::client::run(cfg)
            .await
            .map_err(anyhow::Error::new)
            .context("client"),
    }
}

/// `BOX_EGRESS_RELAY_HOSTS` (and `--allow-host`): a value that names nothing is an error naming it.
fn merge_allowlist(csv: Option<String>, extra: Vec<String>) -> Result<Allowlist> {
    let mut raw = csv.unwrap_or_default();
    for host in extra {
        if raw.is_empty() {
            raw = host;
        } else {
            raw.push(',');
            raw.push_str(&host);
        }
    }
    Allowlist::parse(&raw).map_err(|why| anyhow::anyhow!("BOX_EGRESS_RELAY_HOSTS {why}"))
}
