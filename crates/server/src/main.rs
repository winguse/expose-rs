use clap::Parser;
use expose_server::{
    run_server_with_channel_config, CapacityConfig, HeartbeatConfig,
    DEFAULT_MAX_PENDING_MESSAGES_PER_CONNECTION,
};
use std::time::Duration;
use tokio::net::TcpListener;

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "expose-rs server — protocol-neutral TCP tunnel"
)]
struct Args {
    #[arg(long, default_value = "0.0.0.0")]
    host: String,

    #[arg(long, default_value_t = 8080)]
    port: u16,

    #[arg(long)]
    secret_token: String,

    #[arg(long, default_value_t = DEFAULT_MAX_PENDING_MESSAGES_PER_CONNECTION)]
    max_pending_messages_per_connection: usize,

    /// Seconds between WebSocket ping frames sent for heartbeating (0 = disabled).
    #[arg(long, default_value_t = expose_server::DEFAULT_HEARTBEAT_INTERVAL_SECS)]
    heartbeat_interval: u64,

    /// Number of consecutive missed pongs before the tunnel connection is closed.
    #[arg(long, default_value_t = expose_server::DEFAULT_HEARTBEAT_MAX_MISSED)]
    heartbeat_max_missed: u32,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "expose_server=info".into()),
        )
        .init();

    let args = Args::parse();
    let addr = format!("{}:{}", args.host, args.port);
    let listener = TcpListener::bind(&addr).await.expect("Failed to bind");
    run_server_with_channel_config(
        listener,
        args.secret_token,
        CapacityConfig {
            max_pending_messages_per_connection: args.max_pending_messages_per_connection,
        },
        HeartbeatConfig {
            interval: Duration::from_secs(args.heartbeat_interval),
            max_missed: args.heartbeat_max_missed,
        },
    )
    .await;
}
