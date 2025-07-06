//! Handles [`UdpRequest`]

use crate::{
    find_mac, find_network,
    state::{State, UnitSelector},
};
use anyhow::{bail, ensure, Context, Result};
use pixie_shared::{
    ChunkHash, HintPacket, RegistrationInfo, UdpRequest, ACTION_PORT, BODY_LEN, CHUNKS_PORT,
    HINT_PORT, PACKET_LEN,
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
    let mut write_buf = [0; PACKET_LEN];
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
                None => rx.recv().await,
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

        let Some(data) = state
            .get_chunk_cdata(index)
            .with_context(|| format!("get chunk {}", hex::encode(index)))?
        else {
            log::warn!("Chunk {} not found", hex::encode(index));
            continue;
        };

        let num_packets = data.len().div_ceil(BODY_LEN);
        write_buf[..32].clone_from_slice(&index);

        let mut xor = [[0; BODY_LEN]; 32];

        let hosts_cfg = &state.config.hosts;
        let chunks_addr = SocketAddrV4::new(ip, CHUNKS_PORT);

        for index in 0..num_packets {
            write_buf[32..34].clone_from_slice(&(index as u16).to_le_bytes());
            let start = index * BODY_LEN;
            let len = BODY_LEN.min(data.len() - start);
            let body = &data[start..start + len];
            let group = index & 31;
            body.iter()
                .zip(xor[group].iter_mut())
                .for_each(|(a, b)| *b ^= a);
            write_buf[34..34 + len].clone_from_slice(body);

            wait_for = wait_for.max(Instant::now());
            time::sleep_until(wait_for).await;

            let sent_len = socket.send_to(&write_buf[..34 + len], chunks_addr).await?;
            ensure!(sent_len == 34 + len, "Could not send packet");
            wait_for += 8 * (sent_len as u32) * Duration::from_secs(1) / hosts_cfg.broadcast_speed;
        }

        for (index, body) in xor.iter().enumerate().take(num_packets) {
            write_buf[32..34].clone_from_slice(&(index as u16).wrapping_sub(32).to_le_bytes());
            let len = BODY_LEN;
            write_buf[34..34 + len].clone_from_slice(body);

            wait_for = wait_for.max(Instant::now());
            time::sleep_until(wait_for).await;
            let sent_len = socket.send_to(&write_buf[..34 + len], chunks_addr).await?;
            ensure!(sent_len == 34 + len, "Could not send packet");
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

async fn handle_requests(state: &State, socket: &UdpSocket, tx: Sender<[u8; 32]>) -> Result<()> {
    let mut buf = [0; PACKET_LEN];
    loop {
        let (len, addr) = tokio::select! {
            x = socket.recv_from(&mut buf) => x?,
            _ = state.cancel_token.cancelled() => break,
        };
        let req: postcard::Result<UdpRequest> = postcard::from_bytes(&buf[..len]);
        match req {
            Ok(UdpRequest::Discover) => {
                socket.send_to(&[], addr).await?;
            }
            Ok(UdpRequest::ActionProgress(frac, tot)) => {
                let IpAddr::V4(peer_ip) = addr.ip() else {
                    bail!("IPv6 is not supported")
                };
                match find_mac(peer_ip) {
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
                log::warn!("Invalid request from {addr}: {e}");
            }
        }
    }
    Ok(())
}

pub async fn main(state: Arc<State>) -> Result<()> {
    let (_, network) = find_network(state.config.hosts.listen_on)?;

    let (tx, rx) = mpsc::channel(128);
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, ACTION_PORT)).await?;
    log::info!("Listening on {}", socket.local_addr()?);
    socket.set_broadcast(true)?;

    tokio::try_join!(
        broadcast_chunks(&state, &socket, network.broadcast(), rx),
        broadcast_hint(&state, &socket, network.broadcast()),
        handle_requests(&state, &socket, tx),
    )?;

    Ok(())
}
