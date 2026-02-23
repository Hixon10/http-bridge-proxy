use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

const CHANNEL_BUFFER: usize = 256;
const READ_BUF_SIZE: usize = 8192;

pub struct TcpProxy;

impl TcpProxy {
    pub async fn connect(
        target: &str,
        cancel: CancellationToken,
    ) -> Result<(mpsc::Sender<Vec<u8>>, mpsc::Receiver<Vec<u8>>)> {
        let stream = TcpStream::connect(target)
            .await
            .with_context(|| format!("Failed to connect to {target}"))?;

        info!("Connected to {target}");

        let (read_half, write_half) = stream.into_split();

        let (write_tx, write_rx) = mpsc::channel::<Vec<u8>>(CHANNEL_BUFFER);
        let (read_tx, read_rx) = mpsc::channel::<Vec<u8>>(CHANNEL_BUFFER);

        tokio::spawn(Self::reader_task(read_half, read_tx, cancel.clone()));
        tokio::spawn(Self::writer_task(write_half, write_rx, cancel));

        Ok((write_tx, read_rx))
    }

    async fn reader_task(
        mut read_half: tokio::net::tcp::OwnedReadHalf,
        read_tx: mpsc::Sender<Vec<u8>>,
        cancel: CancellationToken,
    ) {
        let mut buf = vec![0u8; READ_BUF_SIZE];
        let reason;

        loop {
            tokio::select! {
                biased;

                _ = cancel.cancelled() => {
                    reason = "cancellation requested";
                    break;
                }

                result = read_half.read(&mut buf) => {
                    match result {
                        Ok(0) => {
                            reason = "TCP EOF";
                            break;
                        }
                        Ok(n) => {
                            let data = buf[..n].to_vec();
                            if read_tx.send(data).await.is_err() {
                                reason = "read channel receiver dropped";
                                break;
                            }
                        }
                        Err(e) => {
                            error!("TCP read error: {e}");
                            reason = "TCP read error";
                            break;
                        }
                    }
                }
            }
        }

        warn!("TCP reader stopping: {reason}");
        cancel.cancel();
    }

    async fn writer_task(
        mut write_half: tokio::net::tcp::OwnedWriteHalf,
        mut write_rx: mpsc::Receiver<Vec<u8>>,
        cancel: CancellationToken,
    ) {
        let reason;

        loop {
            tokio::select! {
                biased;

                _ = cancel.cancelled() => {
                    reason = "cancellation requested";
                    break;
                }

                msg = write_rx.recv() => {
                    match msg {
                        Some(data) => {
                            if let Err(e) = write_half.write_all(&data).await {
                                error!("TCP write error: {e}");
                                reason = "TCP write error";
                                break;
                            }
                            if let Err(e) = write_half.flush().await {
                                error!("TCP flush error: {e}");
                                reason = "TCP flush error";
                                break;
                            }
                        }
                        None => {
                            reason = "write channel sender dropped";
                            break;
                        }
                    }
                }
            }
        }

        warn!("TCP writer stopping: {reason}");
        cancel.cancel();
    }
}
