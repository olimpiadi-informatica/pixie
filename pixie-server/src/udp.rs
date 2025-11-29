//! Handles [`UdpRequest`]

use crate::{
    find_mac,
    state::{State, UnitSelector},
};
use anyhow::{ensure, Context, Result};
use futures::FutureExt;
use ipnet::Ipv4Net;
use pixie_shared::{
    chunk_codec::Encoder, ChunkHash, HintPacket, RegistrationInfo, UdpRequest, ACTION_PORT,
    CHUNKS_PORT, HINT_PORT, UDP_BODY_LEN,
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
    ip: Ipv4Addr,
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

        let hosts_cfg = &state.config.hosts;
        let chunks_addr = SocketAddrV4::new(ip, CHUNKS_PORT);

        let mut encoder = Encoder::new(cdata);
        write_buf[..32].clone_from_slice(&index);
        while let Some(len) = encoder.next_packet(&mut write_buf[32..]) {
            time::sleep_until(wait_for).await;

            let sent_len = socket.send_to(&write_buf[..32 + len], chunks_addr).await?;
            ensure!(sent_len == 32 + len, "Could not send packet");
            wait_for += 8 * (sent_len as u32) * Duration::from_secs(1) / hosts_cfg.broadcast_speed;
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

    let units = state.get_units(UnitSelector::Group(
        *state.config.groups.get_by_first(&last.group).unwrap(),
    ));
    let positions = units
        .into_iter()
        .map(|unit| (unit.row, unit.col))
        .collect::<Vec<_>>();

    if last.row == 0 {
        if let Some(&(r, c)) = positions.iter().max() {
            last.row = r;
            last.col = c;
        }
    }

    let (mrow, mcol) = positions
        .iter()
        .fold((0, 0), |(r1, c1), &(r2, c2)| (r1.max(r2), c1.max(c2)));

    let (row, col) = match mrow {
        0 => (1, 1),
        1 => (1, mcol + 1),
        _ => (last.row + last.col / mcol, last.col % mcol + 1),
    };

    Ok(RegistrationInfo {
        group: last.group.clone(),
        row,
        col,
        image: last.image.clone(),
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
        let peer_ip = match peer_addr.ip() {
            IpAddr::V4(ip) => ip,
            _ => panic!(),
        };
        let Some((_, tx)) = net_tx.iter().find(|(net, _)| net.contains(&peer_ip)) else {
            continue;
        };
        let req: postcard::Result<UdpRequest> = postcard::from_bytes(&buf[..len]);
        match req {
            Ok(UdpRequest::Discover) => {
                socket.send_to(&[], peer_addr).await?;
            }
            Ok(UdpRequest::ActionProgress(frac, tot)) => {
                match find_mac(peer_addr.ip()) {
                    Ok(peer_mac) => {
                        state.set_unit_progress(UnitSelector::MacAddr(peer_mac), Some((frac, tot)));
                    }
                    Err(err) => {
                        log::error!("Error handling udp packet: {err}");
                    }
                };
            }
            Ok(UdpRequest::RequestChunks(chunks)) => {
                for hash in chunks {
                    tx.send(hash).await?;
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
    let (net_tx, net_rx): (_, Vec<_>) = state
        .config
        .hosts
        .interfaces
        .iter()
        .map(|iface| {
            let (tx, rx) = mpsc::channel(128);
            ((iface.network, tx), (iface.network, rx))
        })
        .unzip();

    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, ACTION_PORT)).await?;
    log::info!("Listening on {}", socket.local_addr()?);
    socket.set_broadcast(true)?;

    let mut tasks = vec![handle_requests(&state, &socket, net_tx).boxed()];

    for (network, rx) in net_rx {
        tasks.push(broadcast_chunks(&state, &socket, network.broadcast(), rx).boxed());
        tasks.push(broadcast_hint(&state, &socket, network.broadcast()).boxed());
    }

    futures::future::try_join_all(tasks).await?;

    Ok(())
}
