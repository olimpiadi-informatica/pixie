use std::{
    collections::BTreeSet,
    fs,
    net::{IpAddr, Ipv4Addr, SocketAddrV4},
    ops::Bound,
};

use tokio::{
    net::UdpSocket,
    sync::mpsc::{self, Receiver, Sender},
    time::{self, Duration, Instant},
};

use anyhow::{anyhow, bail, ensure, Result};

use pixie_shared::{
    to_hex, DhcpMode, HintPacket, Station, UdpRequest, ACTION_PORT, BODY_LEN, PACKET_LEN,
};

use crate::{find_mac, find_network, State};

async fn broadcast_chunks(
    state: &State,
    socket: &UdpSocket,
    ip: Ipv4Addr,
    mut rx: Receiver<[u8; 32]>,
) -> Result<()> {
    let mut queue = BTreeSet::<[u8; 32]>::new();
    let mut write_buf = [0; PACKET_LEN];
    let mut wait_for = Instant::now();
    let mut index = [0; 32];

    loop {
        match rx.recv().await {
            Some(hash) => queue.insert(hash),
            None => break,
        };

        wait_for = wait_for.max(Instant::now());
        loop {
            while let Ok(hash) = rx.try_recv() {
                queue.insert(hash);
            }

            let hash = queue
                .range((Bound::Excluded(index), Bound::Unbounded))
                .next()
                .or_else(|| queue.iter().next());

            let Some(hash) = hash else {
                break;
            };

            index = *hash;
            queue.remove(&index);

            let filename = state.storage_dir.join("chunks").join(to_hex(&index));
            let data = match fs::read(&filename) {
                Ok(data) => data,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    log::warn!("chunk {} not found", to_hex(&index));
                    continue;
                }
                Err(e) => Err(e)?,
            };

            let num_packets = (data.len() + BODY_LEN - 1) / BODY_LEN;
            write_buf[..32].clone_from_slice(&index);

            let mut xor = [[0; BODY_LEN]; 32];

            let hosts_cfg = &state.config.hosts;
            let chunks_addr = SocketAddrV4::new(ip, hosts_cfg.chunks_port);

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

                time::sleep_until(wait_for).await;

                let sent_len = socket.send_to(&write_buf[..34 + len], chunks_addr).await?;
                ensure!(sent_len == 34 + len, "Could not send packet");
                wait_for +=
                    8 * (sent_len as u32) * Duration::from_secs(1) / hosts_cfg.bits_per_second;
            }

            for index in 0..32.min(num_packets) {
                write_buf[32..34].clone_from_slice(&(index as u16).wrapping_sub(32).to_le_bytes());
                let len = BODY_LEN;
                let body = &xor[index];
                write_buf[34..34 + len].clone_from_slice(body);

                time::sleep_until(wait_for).await;
                let sent_len = socket.send_to(&write_buf[..34 + len], chunks_addr).await?;
                ensure!(sent_len == 34 + len, "Could not send packet");
                wait_for +=
                    8 * (sent_len as u32) * Duration::from_secs(1) / hosts_cfg.bits_per_second;
            }
        }
    }

    Ok(())
}

fn compute_hint(state: &State) -> Result<Station> {
    let last = &*state
        .last
        .lock()
        .map_err(|_| anyhow!("hint lock poisoned"))?;

    let positions = state
        .units
        .lock()
        .unwrap()
        .iter()
        .filter(|unit| unit.group == *state.config.groups.get_by_first(&last.group).unwrap())
        .map(|unit| (unit.row, unit.col))
        .collect::<Vec<_>>();

    let (mrow, mcol) = positions
        .iter()
        .fold((0, 0), |(r1, c1), &(r2, c2)| (r1.max(r2), c1.max(c2)));

    let (row, col) = match mrow {
        0 => (1, 1),
        1 => (1, mcol + 1),
        _ => (last.row + (last.col + 1) / mcol, (last.col + 1) % mcol),
    };

    Ok(Station {
        group: last.group.clone(),
        row,
        col,
        image: last.image.clone(),
    })
}

async fn broadcast_hint(state: &State, socket: &UdpSocket, ip: Ipv4Addr) -> Result<()> {
    loop {
        let hint = HintPacket {
            station: compute_hint(state)?,
            images: state.config.images.clone(),
            groups: state.config.groups.clone(),
        };
        let data = postcard::to_allocvec(&hint)?;
        let hint_addr = SocketAddrV4::new(ip, state.config.hosts.hint_port);
        socket.send_to(&data, hint_addr).await?;
        time::sleep(Duration::from_secs(1)).await;
    }
}

async fn handle_requests(state: &State, socket: &UdpSocket, tx: Sender<[u8; 32]>) -> Result<()> {
    let mut buf = [0; PACKET_LEN];
    loop {
        let (len, addr) = socket.recv_from(&mut buf).await?;
        let req: postcard::Result<UdpRequest> = postcard::from_bytes(&buf[..len]);
        match req {
            Ok(UdpRequest::Discover) => {
                socket.send_to(&[], addr).await?;
            }
            Ok(UdpRequest::ActionProgress(frac, tot)) => {
                let IpAddr::V4(peer_ip) = addr.ip() else {
                    bail!("IPv6 is not supported")
                };
                let peer_mac = find_mac(peer_ip)?;
                let mut units = state.units.lock().unwrap();
                let Some(unit) = units.iter_mut().find(|unit| unit.mac == peer_mac) else {
                    log::warn!("Got AP from unknown unit");
                    continue;
                };
                unit.curr_progress = Some((frac, tot));
            }
            Ok(UdpRequest::RequestChunks(chunks)) => {
                for hash in chunks {
                    tx.send(hash).await?;
                }
            }
            Err(e) => {
                log::warn!("Invalid request from {}: {}", addr, e);
            }
        }
    }
}

pub async fn main(state: &State) -> Result<()> {
    let network = find_network(match state.config.dhcp.mode {
        DhcpMode::Static => Ipv4Addr::new(10, pixie_shared::UNASSIGNED_GROUP_ID, 0, 1),
        DhcpMode::Proxy(ip) => ip,
    })?;

    let (tx, rx) = mpsc::channel(128);
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, ACTION_PORT)).await?;
    log::info!("Listening on {}", socket.local_addr()?);
    socket.set_broadcast(true)?;

    tokio::try_join!(
        broadcast_chunks(state, &socket, network.broadcast(), rx),
        broadcast_hint(state, &socket, network.broadcast()),
        handle_requests(state, &socket, tx),
    )?;

    Ok(())
}
