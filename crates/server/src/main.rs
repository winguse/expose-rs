use clap::Parser;
use expose_server::{
    run_server_with_channel_config, CapacityConfig,
    DEFAULT_MAX_INFLIGHT_FROM_TUNNEL_PER_CONNECTION, DEFAULT_MAX_INFLIGHT_TO_TUNNEL_PER_CONNECTION,
};
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

    #[arg(long, default_value_t = DEFAULT_MAX_INFLIGHT_TO_TUNNEL_PER_CONNECTION)]
    max_inflight_to_tunnel_per_connection: usize,

    #[arg(long, default_value_t = DEFAULT_MAX_INFLIGHT_FROM_TUNNEL_PER_CONNECTION)]
    max_inflight_from_tunnel_per_connection: usize,
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
            max_inflight_to_tunnel_per_connection: args.max_inflight_to_tunnel_per_connection,
            max_inflight_from_tunnel_per_connection: args.max_inflight_from_tunnel_per_connection,
        },
    )
    .await;
}
