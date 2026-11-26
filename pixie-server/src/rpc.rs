//! Handles reliable client-to-server UDP RPCs and SACK bulk uploads on [`RPC_PORT`].

use crate::{
    find_mac,
    state::{State, UnitSelector},
};
use anyhow::{Context, Result};
use macaddr::MacAddr6;
use pixie_shared::{
    Image, RPC_PORT,
    rpc::{MAX_RPC_PACKET_LEN, RpcPacket, SackReceiver, SmallRpc, UploadKind, truncate_err},
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{net::UdpSocket, sync::mpsc, time};

const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(15);
const CLEANUP_INTERVAL: Duration = Duration::from_secs(5);
const MAX_CONCURRENT_UPLOADS: usize = 8;
const MAX_RECENT_DONE: usize = 32;

struct UploadSession {
    kind: UploadKind,
    peer: SocketAddr,
    receiver: SackReceiver,
    last_active: Instant,
}

struct CompletedBlob {
    kind: UploadKind,
    peer: SocketAddr,
    data: Vec<u8>,
}

enum BlobAction {
    ReplayCached,
    Ignore,
    Reject(&'static str),
    Processed {
        ack: Option<(u16, u64)>,
        completed: Option<CompletedBlob>,
    },
}

struct BlobPacket<'a> {
    nonce: u64,
    req_id: u32,
    kind: UploadKind,
    total_len: u32,
    block_idx: u16,
    data: &'a [u8],
}

#[derive(Default)]
struct PeerState {
    uploads: HashMap<(u64, u32), UploadSession>,
    finalizing: HashSet<(u64, u32)>,
    recent_done: VecDeque<(u64, u32, Vec<u8>)>,
}

impl PeerState {
    fn find_done(&self, nonce: u64, req_id: u32) -> Option<&[u8]> {
        self.recent_done
            .iter()
            .rev()
            .find(|(dn, dr, _)| *dn == nonce && *dr == req_id)
            .map(|(_, _, pkt)| pkt.as_slice())
    }

    fn record_done(&mut self, nonce: u64, req_id: u32, packet: Vec<u8>) {
        if let Some(pos) = self
            .recent_done
            .iter()
            .position(|(dn, dr, _)| *dn == nonce && *dr == req_id)
        {
            self.recent_done.remove(pos);
        } else if self.recent_done.len() >= MAX_RECENT_DONE {
            self.recent_done.pop_front();
        }
        self.recent_done.push_back((nonce, req_id, packet));
    }

    fn finish_done(&mut self, nonce: u64, req_id: u32, packet: Vec<u8>) {
        self.finalizing.remove(&(nonce, req_id));
        self.record_done(nonce, req_id, packet);
    }

    fn cleanup_idle(&mut self, now: Instant) {
        self.uploads
            .retain(|_, ul| now.saturating_duration_since(ul.last_active) <= SESSION_IDLE_TIMEOUT);
    }

    fn on_blob_data(
        &mut self,
        pkt: BlobPacket<'_>,
        peer_addr: SocketAddr,
        now: Instant,
    ) -> Result<BlobAction, &'static str> {
        let key = (pkt.nonce, pkt.req_id);
        if self.find_done(pkt.nonce, pkt.req_id).is_some() {
            return Ok(BlobAction::ReplayCached);
        }
        if self.finalizing.contains(&key) {
            return Ok(BlobAction::Ignore);
        }

        if !self.uploads.contains_key(&key) {
            if self.uploads.len() >= MAX_CONCURRENT_UPLOADS {
                return Ok(BlobAction::Ignore);
            }
            let receiver = match SackReceiver::new(pkt.total_len as usize) {
                Ok(r) => r,
                Err(msg) => return Ok(BlobAction::Reject(msg)),
            };
            self.uploads.insert(
                key,
                UploadSession {
                    kind: pkt.kind,
                    peer: peer_addr,
                    receiver,
                    last_active: now,
                },
            );
        }

        let ul = self.uploads.get_mut(&key).unwrap();
        ul.peer = peer_addr;
        ul.last_active = now;
        let should_ack = ul.receiver.on_data(pkt.block_idx, pkt.data)?;
        let ack = should_ack.then(|| (ul.receiver.ack_base(), ul.receiver.sack_mask()));
        let completed = if ul.receiver.is_complete() {
            let mut ul = self.uploads.remove(&key).unwrap();
            let data = ul.receiver.take_data().unwrap();
            self.finalizing.insert(key);
            Some(CompletedBlob {
                kind: ul.kind,
                peer: ul.peer,
                data,
            })
        } else {
            None
        };

        Ok(BlobAction::Processed { ack, completed })
    }
}

struct DoneMsg {
    mac: MacAddr6,
    nonce: u64,
    req_id: u32,
    peer: SocketAddr,
    packet: Vec<u8>,
}

fn handle_small_rpc(state: &State, req: SmallRpc, peer_mac: MacAddr6) -> Result<Vec<u8>> {
    Ok(match req {
        SmallRpc::GetAction => postcard::to_allocvec(&state.get_unit_action(peer_mac))?,
        SmallRpc::ActionComplete => {
            state.unit_complete_action(UnitSelector::MacAddr(peer_mac));
            Vec::new()
        }
        SmallRpc::Register(station, stat) => {
            state.register_unit(peer_mac, station.clone(), stat)?;
            state.set_registration_hint(station);
            Vec::new()
        }
        SmallRpc::HasChunks(hashes) => {
            let res: Vec<bool> = hashes.into_iter().map(|h| state.has_chunk(h)).collect();
            postcard::to_allocvec(&res)?
        }
        SmallRpc::GetImageInfo => {
            let unit = state.get_unit(peer_mac).context("Unit not found")?;
            let info = state
                .get_image_info(&unit.image)?
                .context("Image not yet stored")?;
            postcard::to_allocvec(&info)?
        }
    })
}

pub async fn main(state: Arc<State>) -> Result<()> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, RPC_PORT)).await?;
    log::info!("RPC listening on {}", socket.local_addr()?);

    let mut rx_buf = [0u8; MAX_RPC_PACKET_LEN];
    let mut tx_buf = [0u8; MAX_RPC_PACKET_LEN];
    let mut peers: HashMap<MacAddr6, PeerState> = HashMap::new();

    let (done_tx, mut done_rx) = mpsc::channel::<DoneMsg>(128);
    let mut cleanup_tick = time::interval(CLEANUP_INTERVAL);
    cleanup_tick.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            res = socket.recv_from(&mut rx_buf) => {
                let (len, peer_addr) = res?;
                let pkt = match postcard::from_bytes::<RpcPacket>(&rx_buf[..len]) {
                    Ok(pkt) => pkt,
                    Err(e) => {
                        log::warn!("Invalid RPC packet from {peer_addr}: {e}");
                        continue;
                    }
                };
                let peer_mac = match find_mac(&state, peer_addr.ip()).await {
                    Ok(mac) => mac,
                    Err(err) => {
                        log::error!("Error resolving MAC for RPC from {peer_addr}: {err}");
                        continue;
                    }
                };
                let now = Instant::now();
                let peer = peers.entry(peer_mac).or_default();

                match pkt {
                    RpcPacket::SmallReq { nonce, req_id, op } => {
                        if let Some(cached) = peer.find_done(nonce, req_id) {
                            socket.send_to(cached, peer_addr).await?;
                            continue;
                        }
                        let resp = match handle_small_rpc(&state, op, peer_mac) {
                            Ok(body) => postcard::to_allocvec(&RpcPacket::SmallResp {
                                req_id,
                                ok: true,
                                payload: &body,
                            })?,
                            Err(e) => {
                                let msg = e.to_string();
                                log::error!("RPC error from {peer_addr}: {msg}");
                                postcard::to_allocvec(&RpcPacket::SmallResp {
                                    req_id,
                                    ok: false,
                                    payload: truncate_err(&msg).as_bytes(),
                                })?
                            }
                        };
                        socket.send_to(&resp, peer_addr).await?;
                        peer.record_done(nonce, req_id, resp);
                    }
                    RpcPacket::BlobData {
                        nonce,
                        req_id,
                        kind,
                        total_len,
                        block_idx,
                        data,
                    } => {
                        match peer.on_blob_data(
                            BlobPacket {
                                nonce,
                                req_id,
                                kind,
                                total_len,
                                block_idx,
                                data,
                            },
                            peer_addr,
                            now,
                        ) {
                            Ok(BlobAction::ReplayCached) => {
                                if let Some(cached) = peer.find_done(nonce, req_id) {
                                    socket.send_to(cached, peer_addr).await?;
                                }
                            }
                            Ok(BlobAction::Ignore) => {}
                            Ok(BlobAction::Reject(msg)) => {
                                log::warn!("Rejecting blob ({total_len} B) from {peer_addr}: {msg}");
                                let done = postcard::to_slice(
                                    &RpcPacket::BlobDone {
                                        req_id,
                                        err: Some(msg),
                                    },
                                    &mut tx_buf,
                                )?;
                                socket.send_to(done, peer_addr).await?;
                            }
                            Ok(BlobAction::Processed { ack, completed }) => {
                                if let Some((ack_base, sack_mask)) = ack {
                                    let ack_pkt = postcard::to_slice(
                                        &RpcPacket::BlobAck {
                                            req_id,
                                            ack_base,
                                            sack_mask,
                                        },
                                        &mut tx_buf,
                                    )?;
                                    socket.send_to(ack_pkt, peer_addr).await?;
                                }
                                if let Some(blob) = completed {
                                    let state = state.clone();
                                    let done_tx = done_tx.clone();
                                    tokio::task::spawn_blocking(move || {
                                        let res = match blob.kind {
                                            UploadKind::Chunk => state.add_chunk(&blob.data),
                                            UploadKind::Image => {
                                                let image: Image = postcard::from_bytes(&blob.data)?;
                                                let unit = state
                                                    .get_unit(peer_mac)
                                                    .context("Unit not found")?;
                                                state.add_image(unit.image, &image)
                                            }
                                        };
                                        let packet = match res {
                                            Ok(()) => postcard::to_allocvec(&RpcPacket::BlobDone {
                                                req_id,
                                                err: None,
                                            })?,
                                            Err(e) => {
                                                let msg = e.to_string();
                                                log::error!(
                                                    "Error finalizing blob from {}: {msg}",
                                                    blob.peer
                                                );
                                                postcard::to_allocvec(&RpcPacket::BlobDone {
                                                    req_id,
                                                    err: Some(truncate_err(&msg)),
                                                })?
                                            }
                                        };
                                        let _ = done_tx.blocking_send(DoneMsg {
                                            mac: peer_mac,
                                            nonce,
                                            req_id,
                                            peer: blob.peer,
                                            packet,
                                        });
                                        Ok::<_, anyhow::Error>(())
                                    });
                                }
                            }
                            Err(e) => {
                                log::warn!("Invalid BlobData from {peer_addr}: {e}");
                            }
                        }
                    }
                    RpcPacket::SmallResp { .. }
                    | RpcPacket::BlobAck { .. }
                    | RpcPacket::BlobDone { .. } => {}
                }
            }
            Some(msg) = done_rx.recv() => {
                let peer = peers.entry(msg.mac).or_default();
                socket.send_to(&msg.packet, msg.peer).await?;
                peer.finish_done(msg.nonce, msg.req_id, msg.packet);
            }
            _ = cleanup_tick.tick() => {
                let now = Instant::now();
                for peer in peers.values_mut() {
                    peer.cleanup_idle(now);
                }
            }
            _ = state.cancel_token.cancelled() => break,
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pixie_shared::rpc::RPC_BLOCK_SIZE;

    fn test_addr() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 40000))
    }

    #[test]
    fn concurrent_uploads_out_of_order_done_and_recent_replay() {
        let mut peer = PeerState::default();
        let addr = test_addr();
        let now = Instant::now();
        let len = (RPC_BLOCK_SIZE * 2) as u32;
        let b0 = vec![0x11u8; RPC_BLOCK_SIZE];
        let b1 = vec![0x22u8; RPC_BLOCK_SIZE];

        let pkt = |nonce, req_id, block_idx, data| BlobPacket {
            nonce,
            req_id,
            kind: UploadKind::Chunk,
            total_len: len,
            block_idx,
            data,
        };

        // Start req 10 (block 0 only) and req 11 (block 0 + block 1, finishes first).
        match peer.on_blob_data(pkt(1, 10, 0, &b0), addr, now).unwrap() {
            BlobAction::Processed { ack, completed } => {
                assert!(ack.is_none());
                assert!(completed.is_none());
            }
            _ => panic!("expected Processed"),
        }

        peer.on_blob_data(pkt(1, 11, 0, &b0), addr, now).unwrap();
        match peer.on_blob_data(pkt(1, 11, 1, &b1), addr, now).unwrap() {
            BlobAction::Processed {
                ack,
                completed: Some(blob),
            } => {
                assert_eq!(ack, Some((2, 0)));
                assert_eq!(blob.data, [b0.as_slice(), b1.as_slice()].concat());
            }
            _ => panic!("expected completed req 11"),
        }
        assert!(peer.uploads.contains_key(&(1, 10)));
        assert!(!peer.uploads.contains_key(&(1, 11)));
        assert!(peer.finalizing.contains(&(1, 11)));

        // Complete req 10 second.
        match peer.on_blob_data(pkt(1, 10, 1, &b1), addr, now).unwrap() {
            BlobAction::Processed {
                completed: Some(_), ..
            } => {}
            _ => panic!("expected completed req 10"),
        }
        assert!(peer.uploads.is_empty());

        // Finalize req 11 and verify replay from recent_done.
        peer.finish_done(1, 11, b"done11".to_vec());
        assert!(!peer.finalizing.contains(&(1, 11)));
        assert!(matches!(
            peer.on_blob_data(pkt(1, 11, 0, &b0), addr, now).unwrap(),
            BlobAction::ReplayCached
        ));
        assert_eq!(peer.find_done(1, 11), Some(b"done11".as_slice()));

        // Verify FIFO eviction past MAX_RECENT_DONE.
        for i in 0..MAX_RECENT_DONE {
            peer.record_done(2, i as u32, vec![i as u8]);
        }
        assert_eq!(peer.recent_done.len(), MAX_RECENT_DONE);
        assert!(peer.find_done(1, 11).is_none());
        assert_eq!(peer.find_done(2, 0), Some([0u8].as_slice()));
    }

    #[test]
    fn concurrent_upload_cap_and_idle_cleanup() {
        let mut peer = PeerState::default();
        let addr = test_addr();
        let t0 = Instant::now();
        let len = (RPC_BLOCK_SIZE * 2) as u32;
        let blk = [0x33u8; RPC_BLOCK_SIZE];
        let pkt = |req_id| BlobPacket {
            nonce: 1,
            req_id,
            kind: UploadKind::Chunk,
            total_len: len,
            block_idx: 0,
            data: &blk,
        };

        for req_id in 0..MAX_CONCURRENT_UPLOADS as u32 {
            assert!(matches!(
                peer.on_blob_data(pkt(req_id), addr, t0).unwrap(),
                BlobAction::Processed {
                    completed: None,
                    ..
                }
            ));
        }

        // Beyond MAX_CONCURRENT_UPLOADS is ignored without failing existing uploads.
        assert!(matches!(
            peer.on_blob_data(pkt(99), addr, t0).unwrap(),
            BlobAction::Ignore
        ));
        assert_eq!(peer.uploads.len(), MAX_CONCURRENT_UPLOADS);

        // Refresh req 0 at t0 + 10s; clean up at t0 + 16s expires the rest.
        let t1 = t0 + Duration::from_secs(10);
        peer.on_blob_data(pkt(0), addr, t1).unwrap();
        let t2 = t0 + SESSION_IDLE_TIMEOUT + Duration::from_secs(1);
        peer.cleanup_idle(t2);
        assert_eq!(peer.uploads.len(), 1);
        assert!(peer.uploads.contains_key(&(1, 0)));

        // A new upload is accepted again once a slot freed.
        assert!(matches!(
            peer.on_blob_data(pkt(99), addr, t2).unwrap(),
            BlobAction::Processed { .. }
        ));
    }

    #[test]
    fn finalizing_ignores_duplicate_poke() {
        let mut peer = PeerState::default();
        let addr = test_addr();
        let now = Instant::now();
        let len = RPC_BLOCK_SIZE as u32;
        let blk = [0x44u8; RPC_BLOCK_SIZE];
        let pkt = BlobPacket {
            nonce: 1,
            req_id: 20,
            kind: UploadKind::Chunk,
            total_len: len,
            block_idx: 0,
            data: &blk,
        };

        assert!(matches!(
            peer.on_blob_data(pkt, addr, now).unwrap(),
            BlobAction::Processed {
                completed: Some(_),
                ..
            }
        ));
        // Poke while finalizing is ignored (does not restart upload or spawn duplicate work).
        assert!(matches!(
            peer.on_blob_data(
                BlobPacket {
                    nonce: 1,
                    req_id: 20,
                    kind: UploadKind::Chunk,
                    total_len: len,
                    block_idx: 0,
                    data: &blk,
                },
                addr,
                now,
            )
            .unwrap(),
            BlobAction::Ignore
        ));

        // Once finalized, poke replays cached BlobDone.
        peer.finish_done(1, 20, b"done20".to_vec());
        assert!(matches!(
            peer.on_blob_data(
                BlobPacket {
                    nonce: 1,
                    req_id: 20,
                    kind: UploadKind::Chunk,
                    total_len: len,
                    block_idx: 0,
                    data: &blk,
                },
                addr,
                now,
            )
            .unwrap(),
            BlobAction::ReplayCached
        ));
        assert_eq!(peer.find_done(1, 20), Some(b"done20".as_slice()));
    }
}
