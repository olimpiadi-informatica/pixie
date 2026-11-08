//! Built-in async read-only TFTP server (RFC 1350 + RFC 2347/2348/2349 `blksize`/`tsize`).

use crate::{listener_ips, state::State};
use anyhow::{Context, Result};
use futures::FutureExt;
use std::{
    net::{Ipv4Addr, SocketAddr},
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{net::UdpSocket, time};
use tokio_util::sync::CancellationToken;

const TFTP_PORT: u16 = 69;
const DEFAULT_BLKSIZE: usize = 512;
const MAX_BLKSIZE: usize = 1468;

const OP_RRQ: u16 = 1;
const OP_DATA: u16 = 3;
const OP_ACK: u16 = 4;
const OP_ERROR: u16 = 5;
const OP_OACK: u16 = 6;

fn resolve_tftp_path(tftp_dir: &Path, raw_name: &str) -> Option<PathBuf> {
    let rel = raw_name.trim_start_matches(['/', '\\']);
    if rel.is_empty() || rel.contains('\\') {
        return None;
    }
    let rel_path = Path::new(rel);
    if rel_path
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return None;
    }
    let candidate = std::fs::canonicalize(tftp_dir.join(rel_path)).ok()?;
    (candidate.starts_with(tftp_dir) && candidate.is_file()).then_some(candidate)
}

async fn send_error(sock: &UdpSocket, peer: SocketAddr, code: u16, msg: &[u8]) {
    let mut pkt = Vec::with_capacity(5 + msg.len());
    pkt.extend_from_slice(&OP_ERROR.to_be_bytes());
    pkt.extend_from_slice(&code.to_be_bytes());
    pkt.extend_from_slice(msg);
    pkt.push(0);
    let _ = sock.send_to(&pkt, peer).await;
}

async fn send_and_wait_ack(
    sock: &UdpSocket,
    cancel: &CancellationToken,
    peer: SocketAddr,
    pkt: &[u8],
    expected_block: u16,
) -> Result<bool> {
    let mut rx_buf = [0u8; 512];
    for _ in 0..5 {
        sock.send_to(pkt, peer).await?;
        let deadline = time::Instant::now() + Duration::from_secs(1);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(false),
                _ = time::sleep_until(deadline) => break,
                res = sock.recv_from(&mut rx_buf) => {
                    let (len, from) = res?;
                    if from != peer || len < 4 {
                        continue;
                    }
                    let op = u16::from_be_bytes([rx_buf[0], rx_buf[1]]);
                    if op == OP_ERROR {
                        // PXE firmware routinely aborts with OP_ERROR after probing `tsize`.
                        return Ok(false);
                    }
                    if op == OP_ACK && u16::from_be_bytes([rx_buf[2], rx_buf[3]]) == expected_block {
                        return Ok(true);
                    }
                }
            }
        }
    }
    Ok(false)
}

async fn handle_rrq(
    tftp_dir: PathBuf,
    cancel: CancellationToken,
    local_ip: Ipv4Addr,
    peer: SocketAddr,
    req: Vec<u8>,
) -> Result<()> {
    let xfer = UdpSocket::bind((local_ip, 0)).await?;

    let mut fields = req[2..].split(|&b| b == 0);
    let Some(filename) = fields.next().and_then(|s| std::str::from_utf8(s).ok()) else {
        send_error(&xfer, peer, 4, b"Malformed RRQ").await;
        return Ok(());
    };
    let Some(mode) = fields.next().and_then(|s| std::str::from_utf8(s).ok()) else {
        send_error(&xfer, peer, 4, b"Missing transfer mode").await;
        return Ok(());
    };
    if !mode.eq_ignore_ascii_case("octet") && !mode.eq_ignore_ascii_case("netascii") {
        send_error(&xfer, peer, 4, b"Unsupported transfer mode").await;
        return Ok(());
    }

    let Some(path) = resolve_tftp_path(&tftp_dir, filename) else {
        send_error(&xfer, peer, 1, b"File not found").await;
        return Ok(());
    };
    let Ok(data) = tokio::fs::read(&path).await else {
        send_error(&xfer, peer, 2, b"Access violation").await;
        return Ok(());
    };

    let mut blksize = DEFAULT_BLKSIZE;
    let mut oack = Vec::new();
    while let (Some(k_raw), Some(v_raw)) = (fields.next(), fields.next()) {
        if k_raw.is_empty() {
            break;
        }
        let (Ok(k), Ok(v)) = (std::str::from_utf8(k_raw), std::str::from_utf8(v_raw)) else {
            continue;
        };
        if k.eq_ignore_ascii_case("blksize") {
            if let Ok(req_bs) = v.parse::<usize>() {
                blksize = req_bs.clamp(8, MAX_BLKSIZE);
                oack.extend_from_slice(b"blksize\0");
                oack.extend_from_slice(blksize.to_string().as_bytes());
                oack.push(0);
            }
        } else if k.eq_ignore_ascii_case("tsize") && v.parse::<u64>().is_ok() {
            oack.extend_from_slice(b"tsize\0");
            oack.extend_from_slice(data.len().to_string().as_bytes());
            oack.push(0);
        }
    }

    if !oack.is_empty() {
        let mut oack_pkt = Vec::with_capacity(2 + oack.len());
        oack_pkt.extend_from_slice(&OP_OACK.to_be_bytes());
        oack_pkt.extend_from_slice(&oack);
        if !send_and_wait_ack(&xfer, &cancel, peer, &oack_pkt, 0).await? {
            return Ok(());
        }
    }

    let total_blocks = (data.len() / blksize) + 1;
    let mut pkt = Vec::with_capacity(4 + blksize);
    for idx in 0..total_blocks {
        let start = idx * blksize;
        let end = (start + blksize).min(data.len());
        let block_num = (idx + 1) as u16;
        pkt.clear();
        pkt.extend_from_slice(&OP_DATA.to_be_bytes());
        pkt.extend_from_slice(&block_num.to_be_bytes());
        pkt.extend_from_slice(&data[start..end]);
        if !send_and_wait_ack(&xfer, &cancel, peer, &pkt, block_num).await? {
            return Ok(());
        }
    }

    Ok(())
}

async fn serve_tftp_ip(state: Arc<State>, tftp_dir: PathBuf, local_ip: Ipv4Addr) -> Result<()> {
    let socket = UdpSocket::bind((local_ip, TFTP_PORT))
        .await
        .with_context(|| format!("bind TFTP on {local_ip}:{TFTP_PORT}"))?;
    log::info!("TFTP listening on {}", socket.local_addr()?);

    let mut buf = [0u8; 1500];
    loop {
        let (len, peer) = tokio::select! {
            res = socket.recv_from(&mut buf) => res?,
            _ = state.cancel_token.cancelled() => break,
        };
        if len < 4 {
            continue;
        }
        let op = u16::from_be_bytes([buf[0], buf[1]]);
        if op == OP_RRQ {
            let req = buf[..len].to_vec();
            let tftp_dir = tftp_dir.clone();
            let cancel = state.cancel_token.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_rrq(tftp_dir, cancel, local_ip, peer, req).await {
                    log::debug!("TFTP transfer to {peer} ended with error: {e}");
                }
            });
        } else {
            send_error(&socket, peer, 4, b"Read-only TFTP server").await;
        }
    }
    Ok(())
}

pub async fn main(state: Arc<State>) -> Result<()> {
    let tftp_dir = std::fs::canonicalize(state.storage_dir.join("tftpboot"))
        .context("canonicalize tftpboot dir")?;
    let ips = listener_ips(&state.config());
    let tasks = ips
        .into_iter()
        .map(|ip| serve_tftp_ip(state.clone(), tftp_dir.clone(), ip).boxed())
        .collect::<Vec<_>>();
    futures::future::try_join_all(tasks).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tftp_rrq_oack_and_path_traversal() {
        let dir = std::env::temp_dir().join(format!("pixie-tftp-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp tftp dir");
        let dir = std::fs::canonicalize(&dir).expect("canonicalize temp dir");
        let payload: Vec<u8> = (0..1500u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.join("boot.efi"), &payload).expect("write boot.efi");

        assert!(resolve_tftp_path(&dir, "../etc/passwd").is_none());
        assert!(resolve_tftp_path(&dir, "sub\\..\\boot.efi").is_none());
        assert!(resolve_tftp_path(&dir, "/boot.efi").is_some());

        let client = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind client");
        let client_addr = client.local_addr().expect("local_addr");

        let mut rrq = Vec::new();
        rrq.extend_from_slice(&OP_RRQ.to_be_bytes());
        rrq.extend_from_slice(b"boot.efi\0octet\0blksize\x001024\0tsize\x000\0");

        let dir_clone = dir.clone();
        let cancel = CancellationToken::new();
        let server_task = tokio::spawn(async move {
            handle_rrq(dir_clone, cancel, Ipv4Addr::LOCALHOST, client_addr, rrq)
                .await
                .expect("handle_rrq");
        });

        let mut buf = [0u8; 2048];
        let (len, tid) = client.recv_from(&mut buf).await.expect("recv OACK");
        assert_eq!(u16::from_be_bytes([buf[0], buf[1]]), OP_OACK);
        assert_eq!(&buf[2..len], b"blksize\x001024\0tsize\x001500\0");

        client
            .send_to(&[0, 4, 0, 0], tid)
            .await
            .expect("ack OACK block 0");

        let mut received = Vec::new();
        for expected_block in 1..=2u16 {
            let (n, _) = client.recv_from(&mut buf).await.expect("recv DATA");
            assert_eq!(u16::from_be_bytes([buf[0], buf[1]]), OP_DATA);
            assert_eq!(u16::from_be_bytes([buf[2], buf[3]]), expected_block);
            received.extend_from_slice(&buf[4..n]);
            let mut ack = [0u8; 4];
            ack[..2].copy_from_slice(&OP_ACK.to_be_bytes());
            ack[2..].copy_from_slice(&expected_block.to_be_bytes());
            client.send_to(&ack, tid).await.expect("send ACK");
        }
        server_task.await.expect("join server task");
        assert_eq!(received, payload);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
