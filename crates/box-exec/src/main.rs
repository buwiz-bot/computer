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

fn main() {
    box_common::make_undumpable();
    init_tracing();
    // First, on this thread alone: capabilities are per thread, and every thread of the runtime
    // takes this one's when it's made. A worker that kept them would hand them to each command.
    if matches!(
        box_exec::users::Users::from_env(),
        box_exec::users::Users::Agents(_)
    ) {
        if let Err(err) = box_exec::users::keep_capabilities_here() {
            tracing::error!(error = %err, "this box's commands would keep its capabilities");
            std::process::exit(1);
        }
    }
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(err) => {
            tracing::error!(error = %err, "no runtime");
            std::process::exit(1);
        }
    };
    runtime.block_on(run());
}

async fn run() {
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
