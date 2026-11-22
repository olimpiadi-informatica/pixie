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
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{net::UdpSocket, sync::mpsc, time};

const SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(15);
const CLEANUP_INTERVAL: Duration = Duration::from_secs(5);

struct UploadSession {
    nonce: u64,
    req_id: u32,
    kind: UploadKind,
    peer: SocketAddr,
    receiver: SackReceiver,
    last_active: Instant,
}

#[derive(Default)]
struct PeerState {
    upload: Option<UploadSession>,
    finalizing: Option<(u64, u32)>,
    last_done: Option<(u64, u32, Vec<u8>)>,
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
                        if let Some((dn, dr, cached)) = &peer.last_done
                            && (*dn, *dr) == (nonce, req_id)
                        {
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
                        peer.last_done = Some((nonce, req_id, resp));
                    }
                    RpcPacket::BlobData {
                        nonce,
                        req_id,
                        kind,
                        total_len,
                        block_idx,
                        data,
                    } => {
                        if let Some((dn, dr, cached)) = &peer.last_done
                            && (*dn, *dr) == (nonce, req_id)
                        {
                            socket.send_to(cached, peer_addr).await?;
                            continue;
                        }
                        if peer.finalizing == Some((nonce, req_id)) {
                            continue;
                        }

                        if peer
                            .upload
                            .as_ref()
                            .is_none_or(|ul| (ul.nonce, ul.req_id) != (nonce, req_id))
                        {
                            let receiver = match SackReceiver::new(total_len as usize) {
                                Ok(r) => r,
                                Err(msg) => {
                                    log::warn!("Rejecting blob ({total_len} B) from {peer_addr}: {msg}");
                                    let done = postcard::to_slice(
                                        &RpcPacket::BlobDone { req_id, err: Some(msg) },
                                        &mut tx_buf,
                                    )?;
                                    socket.send_to(done, peer_addr).await?;
                                    continue;
                                }
                            };
                            peer.upload = Some(UploadSession {
                                nonce,
                                req_id,
                                kind,
                                peer: peer_addr,
                                receiver,
                                last_active: now,
                            });
                        }

                        let ul = peer.upload.as_mut().unwrap();
                        ul.peer = peer_addr;
                        ul.last_active = now;
                        match ul.receiver.on_data(block_idx, data) {
                            Ok(should_ack) => {
                                if should_ack {
                                    let ack = postcard::to_slice(
                                        &RpcPacket::BlobAck {
                                            req_id,
                                            ack_base: ul.receiver.ack_base(),
                                            sack_mask: ul.receiver.sack_mask(),
                                        },
                                        &mut tx_buf,
                                    )?;
                                    socket.send_to(ack, peer_addr).await?;
                                }
                                if ul.receiver.is_complete() {
                                    let mut ul = peer.upload.take().unwrap();
                                    let blob = ul.receiver.take_data().unwrap();
                                    peer.finalizing = Some((nonce, req_id));
                                    let state = state.clone();
                                    let done_tx = done_tx.clone();
                                    tokio::task::spawn_blocking(move || {
                                        let res = match ul.kind {
                                            UploadKind::Chunk => state.add_chunk(&blob),
                                            UploadKind::Image => {
                                                let image: Image = postcard::from_bytes(&blob)?;
                                                let unit = state.get_unit(peer_mac).context("Unit not found")?;
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
                                                log::error!("Error finalizing blob from {}: {msg}", ul.peer);
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
                                            peer: ul.peer,
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
                if peer.finalizing == Some((msg.nonce, msg.req_id)) {
                    peer.finalizing = None;
                }
                socket.send_to(&msg.packet, msg.peer).await?;
                peer.last_done = Some((msg.nonce, msg.req_id, msg.packet));
            }
            _ = cleanup_tick.tick() => {
                let now = Instant::now();
                for peer in peers.values_mut() {
                    if peer
                        .upload
                        .as_ref()
                        .is_some_and(|ul| now.saturating_duration_since(ul.last_active) > SESSION_IDLE_TIMEOUT)
                    {
                        peer.upload = None;
                    }
                }
            }
            _ = state.cancel_token.cancelled() => break,
        }
    }

    Ok(())
}
