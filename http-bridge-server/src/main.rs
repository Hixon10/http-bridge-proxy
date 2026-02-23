mod http_bridge;
mod tcp_proxy;

use anyhow::Result;
use clap::Parser;
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

#[derive(Parser, Debug)]
#[command(name = "tcp-http-bridge")]
#[command(about = "Bridge TCP connections over HTTP polling — auto-discovers sessions")]
struct Args {
    /// Base URL for the HTTP bridge server
    #[arg(short, long, default_value = "http://localhost:8080")]
    bridge_url: String,

    /// Poll interval in milliseconds for per-session data polling
    #[arg(short, long, default_value_t = 500)]
    poll_interval_ms: u64,

    /// Interval in milliseconds for discovering new sessions
    #[arg(short, long, default_value_t = 500)]
    discovery_interval_ms: u64,
}

#[derive(Debug, Deserialize)]
struct SessionInfo {
    session_id: String,
    #[serde(default)]
    target_addr: String,
    #[serde(default)]
    target_port: u16,
}

#[derive(Debug, Deserialize)]
struct ListSessionsResponse {
    sessions: Vec<SessionInfo>,
}

/// Tracks a running session: its cancellation token and join handle.
struct RunningSession {
    cancel: CancellationToken,
    handle: tokio::task::JoinHandle<()>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    info!(
        bridge_url = %args.bridge_url,
        poll_interval_ms = args.poll_interval_ms,
        discovery_interval_ms = args.discovery_interval_ms,
        "Starting tcp-http-bridge (auto-discovery mode)"
    );

    let client = Client::builder().timeout(Duration::from_secs(10)).build()?;

    let discovery_interval = Duration::from_millis(args.discovery_interval_ms);
    let poll_interval = Duration::from_millis(args.poll_interval_ms);
    let base_url = args.bridge_url.trim_end_matches('/').to_string();
    let sessions_url = format!("{base_url}/debug/sessions");

    let mut running: HashMap<String, RunningSession> = HashMap::new();

    loop {
        // ── Cleanup finished sessions ────────────────────────────────
        let finished: Vec<String> = running
            .iter()
            .filter(|(_, rs)| rs.handle.is_finished())
            .map(|(id, _)| id.clone())
            .collect();

        for id in finished {
            if let Some(rs) = running.remove(&id) {
                rs.cancel.cancel();
                info!("Session {id} has finished, removed from tracking");
            }
        }

        // ── Discover new sessions ────────────────────────────────────
        match fetch_sessions(&client, &sessions_url).await {
            Ok(sessions) => {
                for session in sessions {
                    if running.contains_key(&session.session_id) {
                        continue;
                    }

                    let target = if session.target_addr.is_empty() || session.target_port == 0 {
                        warn!(
                            "Session {} has no target address, skipping",
                            session.session_id
                        );
                        continue;
                    } else {
                        format!("{}:{}", session.target_addr, session.target_port)
                    };

                    info!("Discovered new session {} → {target}", session.session_id);

                    let cancel = CancellationToken::new();
                    let cancel_clone = cancel.clone();
                    let session_id = session.session_id.clone();
                    let base_url_clone = base_url.clone();

                    let handle = tokio::spawn(run_session(
                        base_url_clone,
                        session_id.clone(),
                        target,
                        poll_interval,
                        cancel_clone,
                    ));

                    running.insert(session_id, RunningSession { cancel, handle });
                }
            }
            Err(e) => {
                error!("Failed to fetch sessions: {e}");
            }
        }

        tokio::time::sleep(discovery_interval).await;
    }
}

async fn fetch_sessions(client: &Client, url: &str) -> Result<Vec<SessionInfo>> {
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("GET sessions returned HTTP {}", resp.status());
    }
    let body: ListSessionsResponse = resp.json().await?;
    Ok(body.sessions)
}

/// Runs a single session: connects TCP, wires up the HTTP bridge, runs until
/// either side closes or an error occurs.
async fn run_session(
    base_url: String,
    session_id: String,
    target: String,
    poll_interval: Duration,
    cancel: CancellationToken,
) {
    info!("[{session_id}] Connecting to {target}...");

    let (tcp_write_tx, tcp_read_rx) =
        match tcp_proxy::TcpProxy::connect(&target, cancel.clone()).await {
            Ok(channels) => channels,
            Err(e) => {
                error!("[{session_id}] Failed to connect to {target}: {e}");
                cancel.cancel();
                return;
            }
        };

    info!("[{session_id}] TCP connected to {target}");

    let bridge = http_bridge::HttpBridge::new(
        base_url,
        session_id.clone(),
        tcp_write_tx,
        tcp_read_rx,
        poll_interval,
        cancel.clone(),
    );

    if let Err(e) = bridge.run().await {
        error!("[{session_id}] Bridge error: {e}");
    }

    cancel.cancel();
    info!("[{session_id}] Session finished");
}
