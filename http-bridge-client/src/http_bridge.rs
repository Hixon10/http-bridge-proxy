use anyhow::{Context, Result};
use base64::prelude::*;
use serde::Deserialize;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

const MAX_CONSECUTIVE_ERRORS: u32 = 10;

/// Response from GET /debug/<id>/recv
#[derive(Debug, Deserialize)]
struct RecvResponse {
    chunks: Vec<ChunkEntry>,
}

#[derive(Debug, Deserialize)]
struct ChunkEntry {
    base64: String,
}

/// The HTTP bridge uses the **debug API** of the Python server:
///   - GET  /debug/<id>/recv?clear=1  → get data from remote (to send to SOCKS5 client)
///   - POST /debug/<id>/send          → send data to remote (from SOCKS5 client)
pub struct HttpBridge {
    recv_url: String,
    send_url: String,
    /// Send decoded bytes here → they get written to the SOCKS5 client
    to_client_tx: mpsc::Sender<Vec<u8>>,
    /// Receive bytes here ← they were read from the SOCKS5 client
    from_client_rx: mpsc::Receiver<Vec<u8>>,
    poll_interval: Duration,
    client: reqwest::Client,
    cancel: CancellationToken,
}

impl HttpBridge {
    pub fn new(
        base_url: String,
        session_id: String,
        to_client_tx: mpsc::Sender<Vec<u8>>,
        from_client_rx: mpsc::Receiver<Vec<u8>>,
        poll_interval: Duration,
        cancel: CancellationToken,
    ) -> Self {
        let base = base_url.trim_end_matches('/');
        Self {
            recv_url: format!("{base}/debug/{session_id}/recv?clear=1"),
            send_url: format!("{base}/debug/{session_id}/send"),
            to_client_tx,
            from_client_rx,
            poll_interval,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("Failed to build HTTP client"),
            cancel,
        }
    }

    pub async fn run(self) -> Result<()> {
        let HttpBridge {
            recv_url,
            send_url,
            to_client_tx,
            from_client_rx,
            poll_interval,
            client,
            cancel,
        } = self;

        let client_poll = client.clone();
        let client_fwd = client;
        let cancel_poll = cancel.clone();
        let cancel_fwd = cancel.clone();

        // Task 1: Poll GET /debug/<id>/recv → send to SOCKS5 client
        let poll_handle = tokio::spawn(Self::poll_loop(
            client_poll,
            recv_url,
            to_client_tx,
            poll_interval,
            cancel_poll,
        ));

        // Task 2: Read from SOCKS5 client → POST /debug/<id>/send
        let forward_handle = tokio::spawn(Self::forward_loop(
            client_fwd,
            send_url,
            from_client_rx,
            cancel_fwd,
        ));

        tokio::select! {
            res = poll_handle => {
                if let Err(e) = res { error!("Poll task panicked: {e}"); }
            }
            res = forward_handle => {
                if let Err(e) = res { error!("Forward task panicked: {e}"); }
            }
        }

        cancel.cancel();
        Ok(())
    }

    /// Poll the recv endpoint for data from the remote side.
    async fn poll_loop(
        client: reqwest::Client,
        recv_url: String,
        to_client_tx: mpsc::Sender<Vec<u8>>,
        poll_interval: Duration,
        cancel: CancellationToken,
    ) {
        let mut consecutive_errors: u32 = 0;

        loop {
            if cancel.is_cancelled() {
                break;
            }

            match Self::poll_once(&client, &recv_url).await {
                Ok(chunks) => {
                    consecutive_errors = 0;
                    for chunk in chunks {
                        if !chunk.is_empty() {
                            debug!("Bridge recv: {} bytes", chunk.len());
                            if to_client_tx.send(chunk).await.is_err() {
                                warn!("Client channel closed, stopping poll");
                                cancel.cancel();
                                return;
                            }
                        }
                    }
                }
                Err(e) => {
                    consecutive_errors += 1;
                    error!("Poll error ({consecutive_errors}/{MAX_CONSECUTIVE_ERRORS}): {e}");
                    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        error!("Too many consecutive poll errors, giving up");
                        break;
                    }
                }
            }

            tokio::select! {
                _ = tokio::time::sleep(poll_interval) => {}
                _ = cancel.cancelled() => { break; }
            }
        }

        cancel.cancel();
    }

    async fn poll_once(client: &reqwest::Client, url: &str) -> Result<Vec<Vec<u8>>> {
        let resp = client
            .get(url)
            .send()
            .await
            .context("Failed to GET recv endpoint")?;

        if !resp.status().is_success() {
            anyhow::bail!("Recv endpoint returned HTTP {}", resp.status());
        }

        let body: RecvResponse = resp.json().await.context("Failed to parse recv response")?;

        let mut result = Vec::new();
        for entry in body.chunks {
            let bytes = BASE64_STANDARD
                .decode(&entry.base64)
                .context("Failed to base64-decode recv chunk")?;
            if !bytes.is_empty() {
                result.push(bytes);
            }
        }

        Ok(result)
    }

    /// Read from the SOCKS5 client channel and POST to the send endpoint.
    async fn forward_loop(
        client: reqwest::Client,
        send_url: String,
        mut from_client_rx: mpsc::Receiver<Vec<u8>>,
        cancel: CancellationToken,
    ) {
        let mut consecutive_errors: u32 = 0;

        loop {
            let data = tokio::select! {
                biased;
                _ = cancel.cancelled() => { break; }
                msg = from_client_rx.recv() => {
                    match msg {
                        Some(d) => d,
                        None => break,
                    }
                }
            };

            debug!("Bridge send: {} bytes", data.len());

            let b64 = BASE64_STANDARD.encode(&data);
            let payload = serde_json::json!({ "data": b64 });

            match client.post(&send_url).json(&payload).send().await {
                Ok(resp) if resp.status().is_success() => {
                    consecutive_errors = 0;
                }
                Ok(resp) => {
                    consecutive_errors += 1;
                    error!(
                        "Send endpoint returned HTTP {} ({consecutive_errors}/{MAX_CONSECUTIVE_ERRORS})",
                        resp.status()
                    );
                }
                Err(e) => {
                    consecutive_errors += 1;
                    error!("Send POST failed ({consecutive_errors}/{MAX_CONSECUTIVE_ERRORS}): {e}");
                }
            }

            if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                error!("Too many consecutive send errors, giving up");
                break;
            }
        }

        cancel.cancel();
    }
}
