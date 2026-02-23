use anyhow::{Context, Result};
use base64::prelude::*;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

const MAX_CONSECUTIVE_ERRORS: u32 = 10;

#[derive(Debug, Deserialize)]
struct ReadResponse {
    data: Option<String>,
}

#[derive(Debug, Serialize)]
struct WriteRequest {
    data: String,
}

pub struct HttpBridge {
    read_url: String,
    write_url: String,
    tcp_write_tx: mpsc::Sender<Vec<u8>>,
    tcp_read_rx: mpsc::Receiver<Vec<u8>>,
    poll_interval: Duration,
    client: reqwest::Client,
    cancel: CancellationToken,
}

impl HttpBridge {
    pub fn new(
        base_url: String,
        session_id: String,
        tcp_write_tx: mpsc::Sender<Vec<u8>>,
        tcp_read_rx: mpsc::Receiver<Vec<u8>>,
        poll_interval: Duration,
        cancel: CancellationToken,
    ) -> Self {
        let base = base_url.trim_end_matches('/');
        Self {
            read_url: format!("{base}/session/{session_id}/read"),
            write_url: format!("{base}/session/{session_id}/write"),
            tcp_write_tx,
            tcp_read_rx,
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
            read_url,
            write_url,
            tcp_write_tx,
            tcp_read_rx,
            poll_interval,
            client,
            cancel,
        } = self;

        let client_poll = client.clone();
        let client_fwd = client;
        let cancel_poll = cancel.clone();
        let cancel_fwd = cancel.clone();

        let poll_handle = tokio::spawn(Self::poll_loop(
            client_poll,
            read_url,
            tcp_write_tx,
            poll_interval,
            cancel_poll,
        ));

        let forward_handle = tokio::spawn(Self::forward_loop(
            client_fwd,
            write_url,
            tcp_read_rx,
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

        info!("HTTP bridge stopped");
        Ok(())
    }

    async fn poll_loop(
        client: reqwest::Client,
        read_url: String,
        tcp_write_tx: mpsc::Sender<Vec<u8>>,
        poll_interval: Duration,
        cancel: CancellationToken,
    ) {
        let mut consecutive_errors: u32 = 0;

        loop {
            if cancel.is_cancelled() {
                break;
            }

            match Self::poll_once(&client, &read_url).await {
                Ok(Some(bytes)) => {
                    consecutive_errors = 0;
                    debug!("Received {} bytes from HTTP read endpoint", bytes.len());
                    if tcp_write_tx.send(bytes).await.is_err() {
                        warn!("TCP write channel closed, stopping poll loop");
                        break;
                    }
                }
                Ok(None) => {
                    consecutive_errors = 0;
                }
                Err(e) => {
                    consecutive_errors += 1;
                    error!(
                        "Error polling read endpoint ({consecutive_errors}/{MAX_CONSECUTIVE_ERRORS}): {e}"
                    );
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

        warn!("HTTP poll loop stopped");
        cancel.cancel();
    }

    async fn poll_once(client: &reqwest::Client, url: &str) -> Result<Option<Vec<u8>>> {
        let resp = client
            .post(url)
            .send()
            .await
            .context("Failed to POST to read endpoint")?;

        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("Read endpoint returned HTTP {status}");
        }

        let body: ReadResponse = resp
            .json()
            .await
            .context("Failed to parse read response JSON")?;

        match body.data {
            Some(b64) => {
                let bytes = BASE64_STANDARD
                    .decode(&b64)
                    .context("Failed to base64-decode read response data")?;
                Ok(Some(bytes))
            }
            None => Ok(None),
        }
    }

    async fn forward_loop(
        client: reqwest::Client,
        write_url: String,
        mut tcp_read_rx: mpsc::Receiver<Vec<u8>>,
        cancel: CancellationToken,
    ) {
        let mut consecutive_errors: u32 = 0;

        loop {
            let data = tokio::select! {
                biased;
                _ = cancel.cancelled() => { break; }
                msg = tcp_read_rx.recv() => {
                    match msg {
                        Some(d) => d,
                        None => break,
                    }
                }
            };

            debug!("Forwarding {} bytes to HTTP write endpoint", data.len());

            let b64 = BASE64_STANDARD.encode(&data);
            let payload = WriteRequest { data: b64 };

            match client.post(&write_url).json(&payload).send().await {
                Ok(resp) if resp.status().is_success() => {
                    consecutive_errors = 0;
                    debug!("Successfully forwarded data to write endpoint");
                }
                Ok(resp) => {
                    consecutive_errors += 1;
                    error!(
                        "Write endpoint returned HTTP {} ({consecutive_errors}/{MAX_CONSECUTIVE_ERRORS})",
                        resp.status()
                    );
                }
                Err(e) => {
                    consecutive_errors += 1;
                    error!(
                        "Failed to POST to write endpoint ({consecutive_errors}/{MAX_CONSECUTIVE_ERRORS}): {e}"
                    );
                }
            }

            if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                error!("Too many consecutive forward errors, giving up");
                break;
            }
        }

        warn!("HTTP forward loop stopped");
        cancel.cancel();
    }
}
