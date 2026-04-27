use clap::Parser;
use expose_client::{
    run_client_once_with_channel_config, CapacityConfig,
    DEFAULT_MAX_PENDING_MESSAGES_PER_CONNECTION,
};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{info, warn};

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "expose-rs client — protocol-neutral TCP tunnel"
)]
struct Args {
    #[arg(long)]
    server: String,

    #[arg(long)]
    upstream: String,

    #[arg(long, default_value_t = DEFAULT_MAX_PENDING_MESSAGES_PER_CONNECTION)]
    max_pending_messages_per_connection: usize,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "expose_client=info".into()),
        )
        .init();

    let args = Args::parse();
    let mut backoff = Duration::from_secs(1);

    loop {
        info!("Connecting to server: {}", args.server);
        run_client_once_with_channel_config(
            args.server.clone(),
            args.upstream.clone(),
            CapacityConfig {
                max_pending_messages_per_connection: args.max_pending_messages_per_connection,
            },
        )
        .await;
        warn!("Disconnected from server, reconnecting...");
        sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}
