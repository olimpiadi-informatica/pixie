use alloc::boxed::Box;
use alloc::vec::Vec;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use core::time::Duration;

use futures::future::{Either, select};
use pixie_shared::rpc::{
    MAX_RPC_BLOB_LEN, MAX_RPC_PACKET_LEN, RpcPacket, SackSender, SmallRpc, UploadKind,
};
use pixie_shared::{Action, ChunkHash, Image, RPC_PORT, RegistrationInfo, UnitStats};

use crate::os::error::{Error, Result};
use crate::os::executor::Executor;
use crate::os::net::{ETH_PACKET_SIZE, UdpSocket};
use crate::os::timer::{Timer, rdtsc};

static BOOT_NONCE: AtomicU64 = AtomicU64::new(0);
static NEXT_REQ_ID: AtomicU32 = AtomicU32::new(1);

fn session_nonce() -> u64 {
    let cur = BOOT_NONCE.load(Ordering::Relaxed);
    if cur != 0 {
        return cur;
    }
    let val = (rdtsc() as u64) | 1;
    let _ = BOOT_NONCE.compare_exchange(0, val, Ordering::Relaxed, Ordering::Relaxed);
    BOOT_NONCE.load(Ordering::Relaxed)
}

fn alloc_req_id() -> u32 {
    NEXT_REQ_ID.fetch_add(1, Ordering::Relaxed)
}

pub struct RpcClient {
    socket: UdpSocket,
    server_addr: SocketAddrV4,
    nonce: u64,
}

impl RpcClient {
    pub async fn new(server_ip: Ipv4Addr) -> Result<Self> {
        let socket = UdpSocket::bind(None).await?;
        Ok(Self {
            socket,
            server_addr: SocketAddrV4::new(server_ip, RPC_PORT),
            nonce: session_nonce(),
        })
    }

    async fn recv_timeout<'a>(
        &self,
        buf: &'a mut [u8; ETH_PACKET_SIZE],
        dur: Duration,
    ) -> Option<&'a [u8]> {
        let recv = Box::pin(self.socket.recv_from(buf));
        let sleep = Box::pin(Executor::sleep(dur));
        let len = match select(recv, sleep).await {
            Either::Left(((pkt, _), _)) => Some(pkt.len()),
            Either::Right(((), _)) => None,
        }?;
        Some(&buf[..len])
    }

    async fn call_small(&self, op: SmallRpc) -> Result<Vec<u8>> {
        let req_id = alloc_req_id();
        let req_bytes = postcard::to_allocvec(&RpcPacket::SmallReq {
            nonce: self.nonce,
            req_id,
            op,
        })?;
        let mut rx_buf = [0u8; ETH_PACKET_SIZE];

        for _ in 0..50 {
            self.socket.send_to(self.server_addr, &req_bytes).await?;
            let deadline = Timer::micros() + 100_000;
            loop {
                let remaining = deadline - Timer::micros();
                if remaining <= 0 {
                    break;
                }
                let Some(pkt) = self
                    .recv_timeout(&mut rx_buf, Duration::from_micros(remaining as u64))
                    .await
                else {
                    break;
                };
                if let Ok(RpcPacket::SmallResp {
                    req_id: r,
                    ok,
                    payload,
                }) = postcard::from_bytes(pkt)
                    && r == req_id
                {
                    return if ok {
                        Ok(payload.to_vec())
                    } else {
                        Err(Error::msg(
                            core::str::from_utf8(payload).unwrap_or("RPC error"),
                        ))
                    };
                }
            }
        }
        Err(Error::msg("RPC request timed out"))
    }

    async fn send_block(
        &self,
        req_id: u32,
        kind: UploadKind,
        total_len: u32,
        block_idx: u16,
        chunk: &[u8],
        tx_buf: &mut [u8; MAX_RPC_PACKET_LEN],
    ) -> Result<()> {
        let pkt = postcard::to_slice(
            &RpcPacket::BlobData {
                nonce: self.nonce,
                req_id,
                kind,
                total_len,
                block_idx,
                data: chunk,
            },
            tx_buf,
        )?;
        self.socket.send_to(self.server_addr, pkt).await
    }

    pub async fn upload_blob(&self, kind: UploadKind, data: &[u8]) -> Result<()> {
        if data.is_empty() || data.len() > MAX_RPC_BLOB_LEN {
            return Err(Error::msg("invalid blob length"));
        }
        let req_id = alloc_req_id();
        let total_len = data.len() as u32;
        let mut sender = SackSender::new(data.len());
        let mut tx_buf = [0u8; MAX_RPC_PACKET_LEN];
        let mut rx_buf = [0u8; ETH_PACKET_SIZE];
        let mut last_activity_us = Timer::micros();
        let mut last_poke_us = last_activity_us;

        loop {
            while let Some(block_idx) = sender.next_block_to_send(Timer::micros()) {
                let range = sender.block_range(block_idx);
                self.send_block(
                    req_id,
                    kind,
                    total_len,
                    block_idx,
                    &data[range],
                    &mut tx_buf,
                )
                .await?;
            }

            if let Some(pkt) = self
                .recv_timeout(&mut rx_buf, Duration::from_millis(25))
                .await
            {
                match postcard::from_bytes::<RpcPacket>(pkt) {
                    Ok(RpcPacket::BlobAck {
                        req_id: r,
                        ack_base,
                        sack_mask,
                    }) if r == req_id => {
                        let now = Timer::micros();
                        last_activity_us = now;
                        sender.on_ack(ack_base, sack_mask, now);
                    }
                    Ok(RpcPacket::BlobDone { req_id: r, err }) if r == req_id => {
                        return match err {
                            None => Ok(()),
                            Some(msg) => Err(Error::msg(msg)),
                        };
                    }
                    _ => {}
                }
            } else {
                let now = Timer::micros();
                if now - last_activity_us > 30_000_000 {
                    return Err(Error::msg("RPC blob upload timed out"));
                }
                if !sender.is_complete() {
                    sender.on_timeout(now, 25_000);
                } else if now - last_poke_us >= 250_000 {
                    // Re-poke server in case BlobDone was dropped after finalization.
                    let range = sender.block_range(0);
                    self.send_block(req_id, kind, total_len, 0, &data[range], &mut tx_buf)
                        .await?;
                    last_poke_us = now;
                }
            }
        }
    }

    pub async fn get_action(&self) -> Result<Action> {
        let resp = self.call_small(SmallRpc::GetAction).await?;
        Ok(postcard::from_bytes(&resp)?)
    }

    pub async fn complete_action(&self) -> Result<()> {
        let resp = self.call_small(SmallRpc::ActionComplete).await?;
        assert!(resp.is_empty());
        Ok(())
    }

    pub async fn register(&self, info: RegistrationInfo, stats: UnitStats) -> Result<()> {
        let resp = self.call_small(SmallRpc::Register(info, stats)).await?;
        assert!(resp.is_empty());
        Ok(())
    }

    pub async fn has_chunks(&self, hashes: Vec<ChunkHash>) -> Result<Vec<bool>> {
        let resp = self.call_small(SmallRpc::HasChunks(hashes)).await?;
        Ok(postcard::from_bytes(&resp)?)
    }

    pub async fn get_image_info(&self) -> Result<(ChunkHash, usize)> {
        let resp = self.call_small(SmallRpc::GetImageInfo).await?;
        Ok(postcard::from_bytes(&resp)?)
    }

    pub async fn upload_chunk(&self, cdata: &[u8]) -> Result<()> {
        self.upload_blob(UploadKind::Chunk, cdata).await
    }

    pub async fn upload_image(&self, image: &Image) -> Result<()> {
        let bytes = postcard::to_allocvec(image)?;
        self.upload_blob(UploadKind::Image, &bytes).await
    }
}
