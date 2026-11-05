use box_common::BoxConfig;
use box_exec::serve;

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
    let config = match BoxConfig::from_env() {
        Ok(config) => config,
        Err(err) => {
            tracing::error!(error = %err, "invalid configuration");
            std::process::exit(1);
        }
    };
    if let Err(err) = serve(config).await {
        tracing::error!(error = %err, "box-exec exited");
        std::process::exit(1);
    }
}
