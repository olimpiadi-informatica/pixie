use std::{
    collections::BTreeSet,
    fmt::Write,
    fs,
    net::{IpAddr, SocketAddrV4},
    ops::Bound,
    sync::Arc,
};

use tokio::{
    net::UdpSocket,
    sync::mpsc::{self, Receiver, Sender},
    time::{self, Duration, Instant},
};

use anyhow::{anyhow, bail, ensure, Result};
use serde::Deserialize;

use pixie_shared::{Action, Address, Station, BODY_LEN, PACKET_LEN};

use crate::{find_interface_ip, find_mac, ActionKind, State};

#[derive(Debug, Deserialize)]
pub struct Config {
    listen_on: SocketAddrV4,
    chunk_broadcast: SocketAddrV4,
    hint_broadcast: SocketAddrV4,
    bits_per_second: u32,
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::new();
    for byte in bytes {
        write!(s, "{:02x}", byte).unwrap();
    }
    s
}

async fn broadcast_chunks(
    state: &State,
    socket: &UdpSocket,
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
                    eprintln!("ERROR: chunk {} not found", to_hex(&index));
                    continue;
                }
                Err(e) => Err(e)?,
            };

            let num_packets = (data.len() + BODY_LEN - 1) / BODY_LEN;
            write_buf[..32].clone_from_slice(&index);

            let mut xor = [[0; BODY_LEN]; 32];

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
                let sent_len = socket
                    .send_to(&write_buf[..34 + len], state.config.udp.chunk_broadcast)
                    .await?;
                ensure!(sent_len == 34 + len, "Could not send packet");
                wait_for += 8 * (sent_len as u32) * Duration::from_secs(1)
                    / state.config.udp.bits_per_second;
            }

            for index in 0..32.min(num_packets) {
                write_buf[32..34].clone_from_slice(&(index as u16).wrapping_sub(32).to_le_bytes());
                let len = BODY_LEN;
                let body = &xor[index];
                write_buf[34..34 + len].clone_from_slice(body);

                time::sleep_until(wait_for).await;
                let sent_len = socket
                    .send_to(&write_buf[..34 + len], state.config.udp.chunk_broadcast)
                    .await?;
                ensure!(sent_len == 34 + len, "Could not send packet");
                wait_for += 8 * (sent_len as u32) * Duration::from_secs(1)
                    / state.config.udp.bits_per_second;
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
        .read()
        .map_err(|_| anyhow!("units lock poisoned"))?
        .iter()
        .filter(|unit| unit.group == last.group)
        .map(|unit| (unit.row, unit.col))
        .collect::<Vec<_>>();

    let (mrow, mcol) = positions
        .iter()
        .fold((0, 0), |(r1, c1), &(r2, c2)| (r1.max(r2), c1.max(c2)));

    let mut hole = None;
    'find_hole: for row in 1..=mrow {
        for col in 1..=mcol {
            if positions.contains(&(row, col)) {
                continue;
            }

            hole = Some((row, col));
            break 'find_hole;
        }
    }

    let (row, col) = hole.unwrap_or(match mrow {
        0 => (1, 1),
        1 => (1, mcol + 1),
        _ => (mrow + 1, 1),
    });

    Ok(Station {
        group: last.group,
        row,
        col,
        image: last.image.clone(),
    })
}

async fn broadcast_hint(state: &State, socket: &UdpSocket) -> Result<()> {
    loop {
        let hint = compute_hint(state)?;
        let data = serde_json::to_vec(&hint)?;
        socket
            .send_to(&data, state.config.udp.hint_broadcast)
            .await?;
        time::sleep(Duration::from_secs(1)).await;
    }
}

async fn handle_requests(state: &State, socket: &UdpSocket, tx: Sender<[u8; 32]>) -> Result<()> {
    let mut buf = [0; PACKET_LEN];
    loop {
        let (len, addr) = socket.recv_from(&mut buf).await?;
        let buf = &buf[..len];
        if buf == b"GA" {
            let IpAddr::V4(peer_ip) = addr.ip() else {
                bail!("IPv6 is not supported")
            };
            let peer_mac = find_mac(peer_ip)?;
            let units = state
                .units
                .read()
                .map_err(|_| anyhow!("units mutex is poisoned"))?;
            let unit = units.iter().find(|unit| unit.mac == peer_mac);
            let action_kind = unit
                .map(|unit| unit.action)
                .unwrap_or(state.config.boot.unregistered);

            let server_ip = find_interface_ip(peer_ip)?;
            let server_port = state.config.http.listen_on.port();
            let server_loc = Address {
                ip: (
                    server_ip.octets()[0],
                    server_ip.octets()[1],
                    server_ip.octets()[2],
                    server_ip.octets()[3],
                ),
                port: server_port,
            };
            let action = match action_kind {
                ActionKind::Reboot => Action::Reboot,
                ActionKind::Register => Action::Register {
                    server: server_loc,
                    hint_port: state.config.udp.hint_broadcast.port(),
                },
                ActionKind::Push => Action::Push {
                    http_server: server_loc,
                    image: unit.unwrap().image.clone(),
                },
                ActionKind::Pull => Action::Pull {
                    http_server: server_loc,
                    image: unit.unwrap().image.clone(),
                    udp_recv_port: state.config.udp.chunk_broadcast.port(),
                    udp_server: Address {
                        ip: server_loc.ip,
                        port: state.config.udp.listen_on.port(),
                    },
                },
                ActionKind::Wait => Action::Wait,
            };

            let msg = serde_json::to_vec(&action)?;
            socket.send_to(&msg, addr).await?;
        } else if buf.starts_with(b"RB") && (buf.len() - 2) % 32 == 0 {
            for hash in buf[2..].chunks(32) {
                tx.send(hash.try_into().unwrap()).await?;
            }
        }
    }
}

pub async fn main(state: Arc<State>) -> Result<()> {
    let (tx, rx) = mpsc::channel(128);
    let socket = UdpSocket::bind(state.config.udp.listen_on).await?;
    socket.set_broadcast(true)?;

    tokio::try_join!(
        broadcast_chunks(&state, &socket, rx),
        broadcast_hint(&state, &socket),
        handle_requests(&state, &socket, tx),
    )?;

    Ok(())
}
