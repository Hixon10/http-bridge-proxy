mod http_bridge;
mod socks5;

use anyhow::Result;
use clap::Parser;
use tokio::net::TcpListener;
use tracing::{error, info};

#[derive(Parser, Debug)]
#[command(name = "socks5-http-bridge")]
#[command(about = "SOCKS5 proxy that tunnels traffic over an HTTP bridge")]
struct Args {
    /// Local address to listen on for SOCKS5 connections
    #[arg(short, long, default_value = "127.0.0.1:1080")]
    listen: String,

    /// Base URL for the HTTP bridge
    #[arg(short, long, default_value = "http://localhost:8080")]
    bridge_url: String,

    /// Poll interval in milliseconds for reading from the HTTP bridge
    #[arg(short, long, default_value_t = 50)]
    poll_interval_ms: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    info!(
        listen = %args.listen,
        bridge_url = %args.bridge_url,
        poll_interval_ms = args.poll_interval_ms,
        "Starting SOCKS5-HTTP bridge"
    );

    let listener = TcpListener::bind(&args.listen).await?;
    info!("SOCKS5 server listening on {}", args.listen);

    loop {
        let (stream, addr) = listener.accept().await?;
        info!("New connection from {addr}");

        let bridge_url = args.bridge_url.clone();
        let poll_interval = std::time::Duration::from_millis(args.poll_interval_ms);

        tokio::spawn(async move {
            if let Err(e) = socks5::handle_client(stream, &bridge_url, poll_interval).await {
                error!("Connection from {addr} failed: {e}");
            } else {
                info!("Connection from {addr} closed");
            }
        });
    }
}
