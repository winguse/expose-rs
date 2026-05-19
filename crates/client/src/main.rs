use clap::Parser;
use expose_client::{
    run_client_once_with_channel_config, CapacityConfig, HeartbeatConfig,
    DEFAULT_HEARTBEAT_INTERVAL_SECS, DEFAULT_HEARTBEAT_MAX_MISSED,
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

    /// Seconds between WebSocket ping frames sent for heartbeating (0 = disabled).
    #[arg(long, default_value_t = DEFAULT_HEARTBEAT_INTERVAL_SECS)]
    heartbeat_interval: u64,

    /// Number of consecutive missed pongs before the connection is closed.
    #[arg(long, default_value_t = DEFAULT_HEARTBEAT_MAX_MISSED)]
    heartbeat_max_missed: u32,
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

    let heartbeat_config = HeartbeatConfig {
        interval: Duration::from_secs(args.heartbeat_interval),
        max_missed: args.heartbeat_max_missed,
    };

    loop {
        info!("Connecting to server: {}", args.server);
        run_client_once_with_channel_config(
            args.server.clone(),
            args.upstream.clone(),
            CapacityConfig {
                max_pending_messages_per_connection: args.max_pending_messages_per_connection,
            },
            heartbeat_config,
        )
        .await;
        warn!("Disconnected from server, reconnecting...");
        sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}
