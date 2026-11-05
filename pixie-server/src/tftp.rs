//! Built-in read-only TFTP server using the `tftpd` protocol crate.

use crate::state::State;
use anyhow::Result;
use std::{
    net::{Ipv4Addr, SocketAddr},
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tftpd::{ErrorCode, OptionType, Packet, TransferOption};
use tokio::{net::UdpSocket, time::timeout};

const TFTP_PORT: u16 = 69;
const DEFAULT_BLKSIZE: usize = 512;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_RETRIES: usize = 5;

fn resolve_tftp_path(tftp_root: &Path, requested: &str) -> Option<PathBuf> {
    let trimmed = requested
        .trim_end_matches('\0')
        .trim_start_matches(['/', '\\']);
    if trimmed.is_empty() {
        return None;
    }
    let rel = Path::new(trimmed);
    for comp in rel.components() {
        if !matches!(comp, Component::Normal(_)) {
            return None;
        }
    }
    Some(tftp_root.join(rel))
}

async fn send_packet(sock: &UdpSocket, peer: SocketAddr, pkt: &Packet) -> Result<()> {
    if let Ok(bytes) = pkt.serialize() {
        let _ = sock.send_to(&bytes, peer).await;
    }
    Ok(())
}

async fn wait_for_ack(
    sock: &UdpSocket,
    peer: SocketAddr,
    expected_block: u16,
    wait_timeout: Duration,
) -> Result<bool> {
    let mut buf = [0u8; 512];
    let recv_fut = async {
        loop {
            let (len, from) = sock.recv_from(&mut buf).await?;
            if from != peer {
                continue;
            }
            match Packet::deserialize(&buf[..len]) {
                Ok(Packet::Ack(block)) if block == expected_block => return Ok(true),
                Ok(Packet::Error { .. }) => {
                    // Client aborted transfer (e.g. UEFI PXE after receiving OACK with tsize)
                    return Ok(false);
                }
                _ => continue,
            }
        }
    };

    match timeout(wait_timeout, recv_fut).await {
        Ok(res) => res,
        Err(_) => Err(anyhow::anyhow!("TFTP ACK timeout")),
    }
}

pub async fn handle_rrq(
    tftp_root: PathBuf,
    peer: SocketAddr,
    filename: String,
    options: Vec<TransferOption>,
) -> Result<()> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;

    let Some(path) = resolve_tftp_path(&tftp_root, &filename) else {
        send_packet(
            &sock,
            peer,
            &Packet::Error {
                code: ErrorCode::AccessViolation,
                msg: "Invalid file path".to_string(),
            },
        )
        .await?;
        return Ok(());
    };

    let data = match tokio::fs::read(&path).await {
        Ok(d) => d,
        Err(_) => {
            send_packet(
                &sock,
                peer,
                &Packet::Error {
                    code: ErrorCode::FileNotFound,
                    msg: "File not found".to_string(),
                },
            )
            .await?;
            return Ok(());
        }
    };

    let mut blksize = DEFAULT_BLKSIZE;
    let mut req_timeout = DEFAULT_TIMEOUT;
    let mut windowsize = 1usize;
    let mut oack_opts = Vec::new();

    for opt in options {
        match opt.option {
            OptionType::BlockSize => {
                let negotiated = (opt.value as usize).clamp(8, 65464);
                blksize = negotiated;
                oack_opts.push(TransferOption {
                    option: OptionType::BlockSize,
                    value: negotiated as u64,
                });
            }
            OptionType::TransferSize => {
                oack_opts.push(TransferOption {
                    option: OptionType::TransferSize,
                    value: data.len() as u64,
                });
            }
            OptionType::Timeout => {
                let secs = opt.value.clamp(1, 255);
                req_timeout = Duration::from_secs(secs);
                oack_opts.push(TransferOption {
                    option: OptionType::Timeout,
                    value: secs,
                });
            }
            OptionType::WindowSize => {
                let ws = (opt.value as usize).clamp(1, 64);
                windowsize = ws;
                oack_opts.push(TransferOption {
                    option: OptionType::WindowSize,
                    value: ws as u64,
                });
            }
            _ => {}
        }
    }

    if !oack_opts.is_empty() {
        let oack = Packet::Oack(oack_opts);
        let mut acked = false;
        for _ in 0..MAX_RETRIES {
            send_packet(&sock, peer, &oack).await?;
            match wait_for_ack(&sock, peer, 0, req_timeout).await {
                Ok(true) => {
                    acked = true;
                    break;
                }
                Ok(false) => {
                    // Client intentionally aborted after reading OACK (e.g. tsize probe)
                    return Ok(());
                }
                Err(_) => continue,
            }
        }
        if !acked {
            return Ok(());
        }
    }

    let total_blocks = (data.len() / blksize) + 1;
    let mut block_idx = 0usize;

    while block_idx < total_blocks {
        let win_end = (block_idx + windowsize).min(total_blocks);
        let expected_ack = ((win_end - 1) as u16).wrapping_add(1);

        let mut acked = false;
        for _ in 0..MAX_RETRIES {
            for b in block_idx..win_end {
                let start = b * blksize;
                let end = (start + blksize).min(data.len());
                let block_num = (b as u16).wrapping_add(1);
                let pkt = Packet::Data {
                    block_num,
                    data: data[start..end].to_vec(),
                };
                send_packet(&sock, peer, &pkt).await?;
            }

            match wait_for_ack(&sock, peer, expected_ack, req_timeout).await {
                Ok(true) => {
                    acked = true;
                    break;
                }
                Ok(false) => return Ok(()),
                Err(_) => continue,
            }
        }

        if !acked {
            log::warn!("TFTP transfer of {filename} to {peer} timed out");
            return Ok(());
        }

        block_idx = win_end;
    }

    log::debug!(
        "TFTP served {} ({} bytes) to {}",
        filename,
        data.len(),
        peer
    );
    Ok(())
}

pub async fn serve_on_socket(socket: UdpSocket, state: Arc<State>) -> Result<()> {
    let tftp_root = state.storage_dir.join("tftpboot");
    let mut buf = [0u8; 1500];

    loop {
        let (len, peer) = tokio::select! {
            res = socket.recv_from(&mut buf) => res?,
            _ = state.cancel_token.cancelled() => break,
        };

        match Packet::deserialize(&buf[..len]).ok() {
            Some(Packet::Rrq {
                filename, options, ..
            }) => {
                let root = tftp_root.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_rrq(root, peer, filename, options).await {
                        log::warn!("TFTP RRQ error for {peer}: {e}");
                    }
                });
            }
            Some(Packet::Wrq { .. }) => {
                let _ = send_packet(
                    &socket,
                    peer,
                    &Packet::Error {
                        code: ErrorCode::AccessViolation,
                        msg: "Read-only TFTP server".to_string(),
                    },
                )
                .await;
            }
            _ => {}
        }
    }

    Ok(())
}

pub async fn main(state: Arc<State>) -> Result<()> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, TFTP_PORT)).await?;
    log::info!("TFTP listening on {}", socket.local_addr()?);
    serve_on_socket(socket, state).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_tftp_tsize_probe_and_blksize_transfer() {
        let dir = tempfile::tempdir().unwrap();
        let payload: Vec<u8> = (0..4000).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.path().join("pixie-client.efi"), &payload).unwrap();

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client_addr = client.local_addr().unwrap();

        // 1. UEFI PXE tsize=0 size probe followed by Error abort
        let root = dir.path().to_path_buf();
        let probe_handle = tokio::spawn(async move {
            handle_rrq(
                root,
                client_addr,
                "/pixie-client.efi".to_string(),
                vec![
                    TransferOption {
                        option: OptionType::TransferSize,
                        value: 0,
                    },
                    TransferOption {
                        option: OptionType::BlockSize,
                        value: 1468,
                    },
                ],
            )
            .await
            .unwrap();
        });

        let mut buf = [0u8; 2048];
        let (len, server_tid) = client.recv_from(&mut buf).await.unwrap();
        let pkt = Packet::deserialize(&buf[..len]).unwrap();
        assert_eq!(
            pkt,
            Packet::Oack(vec![
                TransferOption {
                    option: OptionType::TransferSize,
                    value: payload.len() as u64,
                },
                TransferOption {
                    option: OptionType::BlockSize,
                    value: 1468,
                },
            ])
        );

        // Abort probe with Error packet just like UEFI PXE
        let abort_bytes = Packet::Error {
            code: ErrorCode::NotDefined,
            msg: "User aborted".to_string(),
        }
        .serialize()
        .unwrap();
        client.send_to(&abort_bytes, server_tid).await.unwrap();
        probe_handle.await.unwrap();

        // 2. Full download with blksize=1468
        let root = dir.path().to_path_buf();
        let dl_handle = tokio::spawn(async move {
            handle_rrq(
                root,
                client_addr,
                "pixie-client.efi".to_string(),
                vec![TransferOption {
                    option: OptionType::BlockSize,
                    value: 1468,
                }],
            )
            .await
            .unwrap();
        });

        let (len, server_tid) = client.recv_from(&mut buf).await.unwrap();
        let pkt = Packet::deserialize(&buf[..len]).unwrap();
        assert_eq!(
            pkt,
            Packet::Oack(vec![TransferOption {
                option: OptionType::BlockSize,
                value: 1468,
            }])
        );

        // ACK block 0 to start data transfer
        client
            .send_to(&Packet::Ack(0).serialize().unwrap(), server_tid)
            .await
            .unwrap();

        let mut received = Vec::new();
        loop {
            let (len, _) = client.recv_from(&mut buf).await.unwrap();
            if let Packet::Data { block_num, data } = Packet::deserialize(&buf[..len]).unwrap() {
                let is_last = data.len() < 1468;
                received.extend_from_slice(&data);
                client
                    .send_to(&Packet::Ack(block_num).serialize().unwrap(), server_tid)
                    .await
                    .unwrap();
                if is_last {
                    break;
                }
            }
        }

        dl_handle.await.unwrap();
        assert_eq!(received, payload);
    }
}
