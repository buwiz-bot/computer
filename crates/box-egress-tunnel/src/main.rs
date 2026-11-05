use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use box_common::ensure_token_strength;
use box_egress_tunnel::config::{load_bearer, url_is_loopback};
use box_egress_tunnel::{Allowlist, TunnelClientConfig, TunnelServerConfig, PROTOCOL_NAME};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "box-egress-tunnel",
    version,
    about = "box-egress-v1: guest CONNECT proxy muxed to a client on the operator's machine"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Listen in the guest (WS + CONNECT proxy + loopback admin).
    Server {
        #[arg(long, env = "BOX_EGRESS_WS_BIND", default_value = "127.0.0.1:8790")]
        ws_bind: SocketAddr,
        #[arg(long, env = "BOX_EGRESS_PROXY_BIND", default_value = "127.0.0.1:8791")]
        proxy_bind: SocketAddr,
        #[arg(long, env = "BOX_EGRESS_ADMIN_BIND", default_value = "127.0.0.1:8792")]
        admin_bind: SocketAddr,
        /// Bearer value. Prefer --bearer-file / BOX_EGRESS_TUNNEL_BEARER_FILE.
        #[arg(long, env = "BOX_EGRESS_TUNNEL_BEARER")]
        bearer: Option<String>,
        #[arg(long, env = "BOX_EGRESS_TUNNEL_BEARER_FILE")]
        bearer_file: Option<PathBuf>,
        /// Comma-separated CONNECT hosts (`*.example.com` ok). Empty = all.
        #[arg(long, env = "BOX_EGRESS_RELAY_HOSTS")]
        relay_hosts: Option<String>,
    },
    /// Attach from the operator's machine and relay outbound TCP.
    Client {
        /// `ws://` or `wss://` URL of the guest tunnel (published 8790, or SSH -L).
        #[arg(long, env = "BOX_EGRESS_CLIENT_URL")]
        url: String,
        #[arg(long, env = "BOX_EGRESS_TUNNEL_BEARER")]
        bearer: Option<String>,
        #[arg(long, env = "BOX_EGRESS_TUNNEL_BEARER_FILE")]
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

#[tokio::main]
async fn main() {
    box_common::make_undumpable();
    init_tracing();
    if let Err(err) = run().await {
        tracing::error!(error = %err, "box-egress-tunnel exited");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Server {
            ws_bind,
            proxy_bind,
            admin_bind,
            bearer,
            bearer_file,
            relay_hosts,
        } => {
            let bearer = load_bearer(bearer_file, bearer, true)?;
            ensure_token_strength(
                "BOX_EGRESS_TUNNEL_BEARER",
                &bearer,
                ws_bind.ip().is_loopback(),
            )?;
            let allowlist = merge_allowlist(relay_hosts, Vec::new());
            tracing::info!(protocol = PROTOCOL_NAME, %ws_bind, %proxy_bind, %admin_bind, "starting server");
            box_egress_tunnel::server::run(TunnelServerConfig {
                ws_bind,
                proxy_bind,
                admin_bind,
                bearer,
                allowlist,
            })
            .await
            .context("server")?;
        }
        Command::Client {
            url,
            bearer,
            bearer_file,
            relay_hosts,
            allow_host,
            reconnect,
        } => {
            let bearer = load_bearer(bearer_file, bearer, false)?;
            let loopback = url_is_loopback(&url);
            ensure_token_strength("BOX_EGRESS_TUNNEL_BEARER", &bearer, loopback)?;
            let allowlist = merge_allowlist(relay_hosts, allow_host);
            let cfg = TunnelClientConfig {
                url,
                bearer,
                allowlist,
                reconnect,
                destination: box_egress_tunnel::destination::DestinationPolicy::from_env(),
            };
            box_egress_tunnel::client::run(cfg)
                .await
                .context("client")?;
        }
    }
    Ok(())
}

fn merge_allowlist(csv: Option<String>, extra: Vec<String>) -> Allowlist {
    let mut raw = csv.unwrap_or_default();
    for host in extra {
        if raw.is_empty() {
            raw = host;
        } else {
            raw.push(',');
            raw.push_str(&host);
        }
    }
    Allowlist::parse(&raw)
}
