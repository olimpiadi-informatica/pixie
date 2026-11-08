//! Handles [`UdpRequest`]

use crate::{
    find_mac, find_network,
    state::{State, UnitSelector},
};
use anyhow::{Context, Result, ensure};
use futures::FutureExt;
use ipnet::Ipv4Net;
use pixie_shared::{
    ACTION_PORT, CHUNKS_PORT, ChunkHash, DhcpMode, HINT_PORT, HintPacket, RegistrationInfo,
    UDP_BODY_LEN, UdpRequest, chunk_codec::Encoder,
};
use std::{
    collections::BTreeSet,
    net::{IpAddr, Ipv4Addr, SocketAddrV4},
    ops::Bound,
    sync::Arc,
};
use tokio::{
    net::UdpSocket,
    sync::mpsc::{self, Receiver, Sender},
    time::{self, Duration, Instant},
};

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
    let Some(mut last) = state.get_registration_hint() else {
        return Ok(RegistrationInfo {
            group: state
                .config
                .groups
                .iter()
                .next()
                .context("there should be at least one group")?
                .0
                .clone(),
            row: 1,
            col: 1,
            image: state
                .config
                .images
                .first()
                .context("there should be at least one image")?
                .clone(),
        });
    };

    let Some(group_id) = state.config.groups.get_by_first(&last.group).copied() else {
        return Ok(RegistrationInfo {
            group: state
                .config
                .groups
                .iter()
                .next()
                .context("there should be at least one group")?
                .0
                .clone(),
            row: 1,
            col: 1,
            image: state
                .config
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
    let image = if state.config.images.contains(&last.image) {
        last.image
    } else {
        state
            .config
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
            images: state.config.images.clone(),
            groups: state.config.groups.clone(),
        };
        let data = postcard::to_allocvec(&hint)?;
        let hint_addr = SocketAddrV4::new(ip, HINT_PORT);
        socket.send_to(&data, hint_addr).await?;
    }
    Ok(())
}

async fn handle_requests(
    state: &State,
    socket: &UdpSocket,
    net_tx: Vec<(Ipv4Net, Sender<[u8; 32]>)>,
) -> Result<()> {
    let mut buf = [0; UDP_BODY_LEN];
    loop {
        let (len, peer_addr) = tokio::select! {
            x = socket.recv_from(&mut buf) => x?,
            _ = state.cancel_token.cancelled() => break,
        };
        let req: postcard::Result<UdpRequest> = postcard::from_bytes(&buf[..len]);
        match req {
            Ok(UdpRequest::Discover) => {
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
            Err(e) => {
                log::warn!("Invalid request from {peer_addr}: {e}");
            }
        }
    }
    Ok(())
}

pub async fn main(state: Arc<State>) -> Result<()> {
    let mut net_tx = Vec::with_capacity(state.config.hosts.interfaces.len());
    let mut net_rx = Vec::with_capacity(state.config.hosts.interfaces.len());
    for iface in &state.config.hosts.interfaces {
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
