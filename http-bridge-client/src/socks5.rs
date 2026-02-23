use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::http_bridge::HttpBridge;

const SOCKS5_VERSION: u8 = 0x05;
const AUTH_NONE: u8 = 0x00;
const CMD_CONNECT: u8 = 0x01;
const ATYP_IPV4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_IPV6: u8 = 0x04;
const REP_SUCCESS: u8 = 0x00;
const REP_GENERAL_FAILURE: u8 = 0x01;
const REP_CMD_NOT_SUPPORTED: u8 = 0x07;

#[derive(Debug, Deserialize)]
struct CreateSessionResponse {
    session_id: Option<String>,
    error: Option<String>,
}

/// Handle a single SOCKS5 client connection end-to-end.
pub async fn handle_client(
    mut stream: TcpStream,
    bridge_url: &str,
    poll_interval: std::time::Duration,
) -> Result<()> {
    // ── Phase 1: Authentication negotiation ──────────────────────────
    negotiate_auth(&mut stream).await?;

    // ── Phase 2: Read the CONNECT request ────────────────────────────
    let (target_addr, target_port) = read_connect_request(&mut stream).await?;

    info!("SOCKS5 CONNECT → {target_addr}:{target_port}");

    // ── Phase 3: Create a session via the HTTP bridge ────────────────
    let session_id = match create_session(bridge_url, &target_addr, target_port).await {
        Ok(id) => {
            info!("Session created: {id} for {target_addr}:{target_port}");
            id
        }
        Err(e) => {
            error!("Failed to create session for {target_addr}:{target_port}: {e}");
            send_connect_reply(&mut stream, REP_GENERAL_FAILURE).await?;
            return Err(e);
        }
    };

    // ── Phase 4: Send success reply ──────────────────────────────────
    send_connect_reply(&mut stream, REP_SUCCESS).await?;

    info!("SOCKS5 tunnel established for {target_addr}:{target_port} via session {session_id}");

    // ── Phase 5: Bidirectional relay ─────────────────────────────────
    relay(stream, bridge_url, &session_id, poll_interval).await?;

    Ok(())
}

/// Call POST /session/create to get a new session ID.
async fn create_session(bridge_url: &str, target_addr: &str, target_port: u16) -> Result<String> {
    let url = format!("{}/session/create", bridge_url.trim_end_matches('/'));

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let payload = serde_json::json!({
        "target_addr": target_addr,
        "target_port": target_port,
    });

    let resp = client
        .post(&url)
        .json(&payload)
        .send()
        .await
        .context("Failed to POST /session/create")?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("POST /session/create returned HTTP {status}: {body}");
    }

    let body: CreateSessionResponse = resp
        .json()
        .await
        .context("Failed to parse /session/create response")?;

    if let Some(err) = body.error {
        bail!("Server refused session: {err}");
    }

    body.session_id
        .context("Server returned success but no session_id")
}

/// SOCKS5 auth negotiation: we only support "no authentication".
async fn negotiate_auth(stream: &mut TcpStream) -> Result<()> {
    let ver = stream.read_u8().await.context("reading version")?;
    if ver != SOCKS5_VERSION {
        bail!("Unsupported SOCKS version: {ver}");
    }

    let nmethods = stream.read_u8().await.context("reading nmethods")? as usize;
    let mut methods = vec![0u8; nmethods];
    stream
        .read_exact(&mut methods)
        .await
        .context("reading methods")?;

    if !methods.contains(&AUTH_NONE) {
        stream.write_all(&[SOCKS5_VERSION, 0xFF]).await?;
        bail!("Client does not support 'no auth'");
    }

    stream.write_all(&[SOCKS5_VERSION, AUTH_NONE]).await?;
    Ok(())
}

/// Read the SOCKS5 CONNECT request and return (address, port).
async fn read_connect_request(stream: &mut TcpStream) -> Result<(String, u16)> {
    let ver = stream.read_u8().await?;
    if ver != SOCKS5_VERSION {
        bail!("Unexpected version in request: {ver}");
    }

    let cmd = stream.read_u8().await?;
    let _rsv = stream.read_u8().await?;
    let atyp = stream.read_u8().await?;

    if cmd != CMD_CONNECT {
        send_connect_reply(stream, REP_CMD_NOT_SUPPORTED).await?;
        bail!("Unsupported command: {cmd} (only CONNECT is supported)");
    }

    let addr = match atyp {
        ATYP_IPV4 => {
            let mut buf = [0u8; 4];
            stream.read_exact(&mut buf).await?;
            format!("{}.{}.{}.{}", buf[0], buf[1], buf[2], buf[3])
        }
        ATYP_DOMAIN => {
            let len = stream.read_u8().await? as usize;
            let mut buf = vec![0u8; len];
            stream.read_exact(&mut buf).await?;
            String::from_utf8(buf).context("Domain is not valid UTF-8")?
        }
        ATYP_IPV6 => {
            let mut buf = [0u8; 16];
            stream.read_exact(&mut buf).await?;
            let segments: Vec<String> = buf
                .chunks(2)
                .map(|c| format!("{:02x}{:02x}", c[0], c[1]))
                .collect();
            segments.join(":")
        }
        _ => {
            send_connect_reply(stream, REP_GENERAL_FAILURE).await?;
            bail!("Unsupported address type: {atyp}");
        }
    };

    let port = stream.read_u16().await?;

    Ok((addr, port))
}

/// Send a SOCKS5 connect reply. BND.ADDR is 0.0.0.0:0.
async fn send_connect_reply(stream: &mut TcpStream, reply: u8) -> Result<()> {
    let response = [SOCKS5_VERSION, reply, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0];
    stream.write_all(&response).await?;
    Ok(())
}

/// Bidirectional relay: SOCKS5 client <-> HTTP bridge.
async fn relay(
    stream: TcpStream,
    bridge_url: &str,
    session_id: &str,
    poll_interval: std::time::Duration,
) -> Result<()> {
    let cancel = CancellationToken::new();

    let (mut read_half, mut write_half) = stream.into_split();

    let (to_client_tx, mut to_client_rx) = mpsc::channel::<Vec<u8>>(256);
    let (to_remote_tx, to_remote_rx) = mpsc::channel::<Vec<u8>>(256);

    let bridge = HttpBridge::new(
        bridge_url.to_string(),
        session_id.to_string(),
        to_client_tx,
        to_remote_rx,
        poll_interval,
        cancel.clone(),
    );

    let cancel_bridge = cancel.clone();
    let bridge_handle = tokio::spawn(async move {
        if let Err(e) = bridge.run().await {
            warn!("HTTP bridge error: {e}");
        }
        cancel_bridge.cancel();
    });

    let cancel_reader = cancel.clone();
    let reader_handle = tokio::spawn(async move {
        let mut buf = vec![0u8; 8192];
        loop {
            tokio::select! {
                biased;
                _ = cancel_reader.cancelled() => break,
                result = read_half.read(&mut buf) => {
                    match result {
                        Ok(0) => break,
                        Ok(n) => {
                            if to_remote_tx.send(buf[..n].to_vec()).await.is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        }
        cancel_reader.cancel();
    });

    let cancel_writer = cancel.clone();
    let writer_handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = cancel_writer.cancelled() => break,
                msg = to_client_rx.recv() => {
                    match msg {
                        Some(data) => {
                            if write_half.write_all(&data).await.is_err() {
                                break;
                            }
                            if write_half.flush().await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
            }
        }
        cancel_writer.cancel();
    });

    let _ = tokio::join!(bridge_handle, reader_handle, writer_handle);

    Ok(())
}
