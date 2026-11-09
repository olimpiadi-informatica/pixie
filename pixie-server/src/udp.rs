//! Handles [`UdpRequest`]

use crate::{
    find_mac, find_network,
    state::{State, UnitSelector},
};
use anyhow::{Context, Result, ensure};
use futures::FutureExt;
use ipnet::Ipv4Net;
use macaddr::MacAddr6;
use pixie_shared::{
    ACTION_PORT, CHUNKS_PORT, ChunkHash, DhcpMode, HINT_PORT, HintPacket, Image, RegistrationInfo,
    RpcRequest, UDP_BODY_LEN, UPLOAD_SLICE_LEN, UdpRequest, chunk_codec::Encoder,
};
use std::{
    collections::{BTreeSet, HashMap},
    net::{IpAddr, Ipv4Addr, SocketAddrV4},
    ops::Bound,
    sync::Arc,
};
use tokio::{
    net::UdpSocket,
    sync::mpsc::{self, Receiver, Sender},
    time::{self, Duration, Instant},
};

/// Largest upload body we're willing to allocate a buffer for (16 MiB).
const MAX_UPLOAD_LEN: usize = 16 * 1024 * 1024;
/// Slice size returned per [`RpcRequest::GetImage`] request.
const GET_IMAGE_SLICE_LEN: usize = 1280;

#[derive(Default)]
struct UploadBuffer {
    id: u16,
    data: Vec<u8>,
    received_slices: Vec<bool>,
}

impl UploadBuffer {
    fn reset_if_new(&mut self, id: u16) -> bool {
        if self.received_slices.is_empty()
            || (id != self.id && (id.wrapping_sub(self.id) as i16) > 0)
        {
            self.id = id;
            self.data.clear();
            self.received_slices.clear();
        }
        self.id == id
    }

    fn clear(&mut self) {
        self.data.clear();
        self.received_slices.clear();
    }
}

async fn broadcast_chunks(
    state: &State,
    socket: &UdpSocket,
    chunks_broadcast: Ipv4Addr,
    broadcast_speed: u32,
    mut rx: Receiver<ChunkHash>,
) -> Result<()> {
    let mut queue = BTreeSet::<ChunkHash>::new();
    let mut write_buf = [0; UDP_BODY_LEN];
    let mut wait_for = Instant::now();
    let mut index = [0; 32];

    loop {
        let get_index = async {
            while let Ok(hash) = rx.try_recv() {
                queue.insert(hash);
            }

            let hash = queue
                .range((Bound::Excluded(index), Bound::Unbounded))
                .next()
                .or_else(|| queue.iter().next())
                .copied();

            match hash {
                Some(hash) => {
                    queue.remove(&hash);
                    Some(hash)
                }
                None => {
                    let hash = rx.recv().await;
                    wait_for = wait_for.max(Instant::now());
                    hash
                }
            }
        };

        tokio::select! {
            hash = get_index => {
                let Some(hash) = hash else {
                    break;
                };
                index = hash;
            }
            _ = state.cancel_token.cancelled() => break,
        };

        let Some(cdata) = state
            .get_chunk_cdata(index)
            .with_context(|| format!("get chunk {}", hex::encode(index)))?
        else {
            log::warn!("Chunk {} not found", hex::encode(index));
            continue;
        };

        let chunks_addr = SocketAddrV4::new(chunks_broadcast, CHUNKS_PORT);

        let mut encoder = Encoder::new(cdata);
        write_buf[..32].clone_from_slice(&index);
        while let Some(len) = encoder.next_packet(&mut write_buf[32..]) {
            time::sleep_until(wait_for).await;

            let sent_len = socket.send_to(&write_buf[..32 + len], chunks_addr).await?;
            ensure!(sent_len == 32 + len, "Could not send packet");
            wait_for += 8 * (sent_len as u32) * Duration::from_secs(1) / broadcast_speed;
        }
    }

    Ok(())
}

fn compute_hint(state: &State) -> Result<RegistrationInfo> {
    let config = state.config();
    let Some(mut last) = state.get_registration_hint() else {
        return Ok(RegistrationInfo {
            group: config
                .groups
                .iter()
                .next()
                .context("there should be at least one group")?
                .0
                .clone(),
            row: 1,
            col: 1,
            image: config
                .images
                .first()
                .context("there should be at least one image")?
                .clone(),
        });
    };

    let Some(group_id) = config.groups.get_by_first(&last.group).copied() else {
        return Ok(RegistrationInfo {
            group: config
                .groups
                .iter()
                .next()
                .context("there should be at least one group")?
                .0
                .clone(),
            row: 1,
            col: 1,
            image: config
                .images
                .first()
                .context("there should be at least one image")?
                .clone(),
        });
    };
    let units = state.get_units(UnitSelector::Group(group_id));
    let positions = units
        .into_iter()
        .map(|unit| (unit.row, unit.col))
        .collect::<Vec<_>>();

    if last.row == 0
        && let Some(&(r, c)) = positions.iter().max()
    {
        last.row = r;
        last.col = c;
    }

    let (mrow, mcol) = positions
        .iter()
        .fold((0, 0), |(r1, c1), &(r2, c2)| (r1.max(r2), c1.max(c2)));

    let (row, col) = match (mrow, mcol) {
        (0, _) | (_, 0) => (1, 1),
        (1, _) => (1, mcol + 1),
        _ => (last.row + last.col / mcol, last.col % mcol + 1),
    };
    let image = if config.images.contains(&last.image) {
        last.image
    } else {
        config
            .images
            .first()
            .context("there should be at least one image")?
            .clone()
    };

    Ok(RegistrationInfo {
        group: last.group,
        row,
        col,
        image,
    })
}

async fn broadcast_hint(state: &State, socket: &UdpSocket, ip: Ipv4Addr) -> Result<()> {
    loop {
        tokio::select! {
            _ = time::sleep(Duration::from_secs(1)) => {}
            _ = state.cancel_token.cancelled() => break,
        }
        let hint = HintPacket {
            station: compute_hint(state)?,
            images: state.config().images.clone(),
            groups: state.config().groups.clone(),
        };
        let data = postcard::to_allocvec(&hint)?;
        let hint_addr = SocketAddrV4::new(ip, HINT_PORT);
        socket.send_to(&data, hint_addr).await?;
    }
    Ok(())
}

fn handle_rpc(
    state: &State,
    seq: u32,
    req: RpcRequest,
    peer_mac: MacAddr6,
    uploads: &mut HashMap<MacAddr6, UploadBuffer>,
) -> Result<Vec<u8>> {
    match req {
        RpcRequest::HasChunk(hash) => {
            let has_chunk = state.has_chunk(hash);
            Ok(postcard::to_allocvec(&(seq, has_chunk))?)
        }
        RpcRequest::GetImage { offset } => {
            let unit = state.get_unit(peer_mac).context("Unit not found")?;
            let image_bytes = state
                .get_image_serialized(&unit.image)?
                .context("Image not yet stored")?;
            let total_len = image_bytes.len() as u32;
            let start = (offset as usize).min(image_bytes.len());
            let end = (start + GET_IMAGE_SLICE_LEN).min(image_bytes.len());
            let slice = image_bytes[start..end].to_vec();
            Ok(postcard::to_allocvec(&(seq, (total_len, slice)))?)
        }
        RpcRequest::Register(station, stat) => {
            state.register_unit(peer_mac, station.clone(), stat)?;
            state.set_registration_hint(station);
            Ok(postcard::to_allocvec(&(seq, ()))?)
        }
        RpcRequest::UploadSync { id, base, count } => {
            ensure!(
                (base as usize).is_multiple_of(UPLOAD_SLICE_LEN),
                "Unaligned upload sync base: {base}"
            );
            let ubuf = uploads.entry(peer_mac).or_default();
            let mut mask = 0u32;
            if ubuf.reset_if_new(id) {
                let base_idx = base as usize / UPLOAD_SLICE_LEN;
                for i in 0..(count as usize).min(32) {
                    if ubuf
                        .received_slices
                        .get(base_idx + i)
                        .copied()
                        .unwrap_or(false)
                    {
                        mask |= 1u32 << i;
                    }
                }
            }
            Ok(postcard::to_allocvec(&(seq, mask))?)
        }
        RpcRequest::CommitChunk { hash, size } => {
            let ubuf = uploads.entry(peer_mac).or_default();
            if !state.has_chunk(hash) {
                ensure!(
                    ubuf.data.len() == size as usize,
                    "Chunk upload size mismatch: got {}, expected {}",
                    ubuf.data.len(),
                    size
                );
                let expected_slices = (size as usize).div_ceil(UPLOAD_SLICE_LEN);
                ensure!(
                    ubuf.received_slices.len() == expected_slices
                        && ubuf.received_slices.iter().all(|&x| x),
                    "Incomplete chunk upload"
                );
                let actual = state.add_chunk(&ubuf.data)?;
                ensure!(
                    actual == hash,
                    "Chunk hash mismatch: expected {}, got {}",
                    hex::encode(hash),
                    hex::encode(actual)
                );
            }
            ubuf.clear();
            Ok(postcard::to_allocvec(&(seq, ()))?)
        }
        RpcRequest::CommitImage { size } => {
            let unit = state.get_unit(peer_mac).context("Unit not found")?;
            let ubuf = uploads.entry(peer_mac).or_default();
            ensure!(
                ubuf.data.len() == size as usize,
                "Image upload size mismatch: got {}, expected {}",
                ubuf.data.len(),
                size
            );
            let expected_slices = (size as usize).div_ceil(UPLOAD_SLICE_LEN);
            ensure!(
                ubuf.received_slices.len() == expected_slices
                    && ubuf.received_slices.iter().all(|&x| x),
                "Incomplete image upload"
            );
            let image: Image = postcard::from_bytes(&ubuf.data)?;
            state.add_image(unit.image, &image)?;
            ubuf.clear();
            Ok(postcard::to_allocvec(&(seq, ()))?)
        }
        RpcRequest::GetAction => {
            uploads.remove(&peer_mac);
            let action = state.get_unit_action(peer_mac);
            Ok(postcard::to_allocvec(&(seq, action))?)
        }
        RpcRequest::ActionComplete => {
            state.unit_complete_action(UnitSelector::MacAddr(peer_mac));
            Ok(postcard::to_allocvec(&(seq, ()))?)
        }
    }
}

async fn handle_requests(
    state: &State,
    socket: &UdpSocket,
    net_tx: Vec<(Ipv4Net, Sender<[u8; 32]>)>,
) -> Result<()> {
    let mut buf = [0; UDP_BODY_LEN];
    let mut uploads: HashMap<MacAddr6, UploadBuffer> = HashMap::new();
    let mut last_rpc: HashMap<MacAddr6, (u32, Vec<u8>)> = HashMap::new();

    loop {
        let (len, peer_addr) = tokio::select! {
            x = socket.recv_from(&mut buf) => x?,
            _ = state.cancel_token.cancelled() => break,
        };
        let req: postcard::Result<UdpRequest> = postcard::from_bytes(&buf[..len]);
        match req {
            Ok(UdpRequest::Discover) => {
                if let IpAddr::V4(v4) = peer_addr.ip()
                    && let Some(mac) = state.lookup_mac(v4).or_else(|| crate::lookup_proc_arp(v4))
                {
                    last_rpc.remove(&mac);
                    uploads.remove(&mac);
                }
                if let Err(e) = socket.send_to(&[], peer_addr).await {
                    log::warn!("Failed to send Discover reply to {peer_addr}: {e}");
                }
            }
            Ok(UdpRequest::ActionProgress(frac, tot)) => {
                match find_mac(state, peer_addr.ip()).await {
                    Ok(peer_mac) => {
                        state.set_unit_progress(UnitSelector::MacAddr(peer_mac), Some((frac, tot)));
                    }
                    Err(err) => {
                        log::error!("Error handling udp packet: {err}");
                    }
                };
            }
            Ok(UdpRequest::RequestChunks(chunks)) => {
                let IpAddr::V4(peer_ip) = peer_addr.ip() else {
                    continue;
                };
                let Some((_, tx)) = net_tx.iter().find(|(net, _)| net.contains(&peer_ip)) else {
                    continue;
                };
                for hash in chunks {
                    let _ = tx.try_send(hash);
                }
            }
            Ok(UdpRequest::UploadSlice { id, offset, data }) => {
                let peer_mac = match find_mac(state, peer_addr.ip()).await {
                    Ok(peer_mac) => peer_mac,
                    Err(err) => {
                        log::error!("Error handling udp upload slice: {err}");
                        continue;
                    }
                };
                let offset = offset as usize;
                let end = offset.saturating_add(data.len());
                if !offset.is_multiple_of(UPLOAD_SLICE_LEN)
                    || data.is_empty()
                    || data.len() > UPLOAD_SLICE_LEN
                    || end > MAX_UPLOAD_LEN
                {
                    log::warn!(
                        "Rejecting invalid upload slice (offset={offset}, len={}) from {peer_addr}",
                        data.len()
                    );
                    continue;
                }
                let ubuf = uploads.entry(peer_mac).or_default();
                if !ubuf.reset_if_new(id) {
                    continue;
                }
                let slice_idx = offset / UPLOAD_SLICE_LEN;
                if ubuf.data.len() < end {
                    ubuf.data.resize(end, 0);
                }
                if ubuf.received_slices.len() <= slice_idx {
                    ubuf.received_slices.resize(slice_idx + 1, false);
                }
                ubuf.data[offset..end].copy_from_slice(&data);
                ubuf.received_slices[slice_idx] = true;
            }
            Ok(UdpRequest::Rpc { seq, req }) => {
                let peer_mac = match find_mac(state, peer_addr.ip()).await {
                    Ok(peer_mac) => peer_mac,
                    Err(err) => {
                        log::error!("Error handling udp rpc: {err}");
                        continue;
                    }
                };
                if let Some((last_seq, last_resp)) = last_rpc.get(&peer_mac)
                    && *last_seq == seq
                {
                    if let Err(e) = socket.send_to(last_resp, peer_addr).await {
                        log::warn!("Failed to resend RPC reply to {peer_addr}: {e}");
                    }
                    continue;
                }
                match handle_rpc(state, seq, req, peer_mac, &mut uploads) {
                    Ok(resp) => {
                        if let Err(e) = socket.send_to(&resp, peer_addr).await {
                            log::warn!("Failed to send RPC reply to {peer_addr}: {e}");
                        }
                        last_rpc.insert(peer_mac, (seq, resp));
                    }
                    Err(err) => {
                        log::error!("Error handling udp rpc from {peer_addr}: {err}");
                    }
                }
            }
            Err(e) => {
                log::warn!("Invalid request from {peer_addr}: {e}");
            }
        }
    }
    Ok(())
}

pub async fn main(state: Arc<State>) -> Result<()> {
    let interfaces = state.config().hosts.interfaces.clone();
    let mut net_tx = Vec::with_capacity(interfaces.len());
    let mut net_rx = Vec::with_capacity(interfaces.len());
    for iface in &interfaces {
        let pixie_net = match iface.dhcp {
            DhcpMode::Static(..) => iface.network,
            DhcpMode::Proxy(proxy_ip) => find_network(proxy_ip)?.1,
        };
        let (tx, rx) = mpsc::channel(2048);
        net_tx.push((pixie_net, tx));
        net_rx.push((pixie_net.broadcast(), iface.broadcast_speed, rx));
    }

    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, ACTION_PORT)).await?;
    log::info!("Listening on {}", socket.local_addr()?);
    socket.set_broadcast(true)?;

    let mut tasks = vec![handle_requests(&state, &socket, net_tx).boxed()];

    for (broadcast_ip, broadcast_speed, rx) in net_rx {
        tasks.push(broadcast_chunks(&state, &socket, broadcast_ip, broadcast_speed, rx).boxed());
        tasks.push(broadcast_hint(&state, &socket, broadcast_ip).boxed());
    }

    futures::future::try_join_all(tasks).await?;

    Ok(())
}
