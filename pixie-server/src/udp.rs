//! Handles [`UdpRequest`]

use crate::{
    find_mac, find_network,
    state::{State, UnitSelector},
};
use anyhow::{Context, Result, ensure};
use futures::FutureExt;
use ipnet::Ipv4Net;
use pixie_shared::{
    ACTION_PORT, CHUNK_PACKET_PREFIX_LEN, CHUNKS_PORT, ChunkHash, DhcpMode, HINT_PORT, HintPacket,
    RegistrationInfo, UDP_BODY_LEN, UdpRequest, chunk_codec::Encoder,
};
use std::{
    collections::{BTreeSet, HashMap},
    io::ErrorKind,
    net::{IpAddr, Ipv4Addr, SocketAddrV4},
    ops::Bound,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    net::UdpSocket,
    sync::mpsc::{self, Receiver, Sender},
    time,
};

async fn broadcast_chunks(
    state: &State,
    socket: &UdpSocket,
    chunks_broadcast: Ipv4Addr,
    mut rx: Receiver<ChunkHash>,
    mut feedback_rx: Receiver<(Ipv4Addr, u32, u32)>,
) -> Result<()> {
    const MIN_RATE: f64 = 500_000.0;
    const INIT_RATE: f64 = 10_000_000.0;
    const MAX_RATE: f64 = 1_125_000_000.0;
    const TICK: Duration = Duration::from_millis(200);
    const IDLE_RESET: Duration = Duration::from_secs(10);
    const MIN_SLEEP: Duration = Duration::from_micros(100);
    const MAX_BURST_LAG: Duration = Duration::from_micros(250);

    let mut queue = BTreeSet::<ChunkHash>::new();
    let mut write_buf = [0; UDP_BODY_LEN];
    let mut wait_for = Instant::now();
    let mut delay = tokio_timerfd::Delay::new(wait_for)?;
    let mut index = [0; 32];
    let mut seq: u32 = 0;

    let mut rate_bps = INIT_RATE;
    let mut tick_start = wait_for;
    let mut tick_bytes = 0u64;
    let mut tick_pkts = 0u64;
    let mut clients = HashMap::<Ipv4Addr, (u64, u64)>::new();

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
                    let idle_start = Instant::now();
                    let hash = rx.recv().await;
                    let now = Instant::now();
                    while feedback_rx.try_recv().is_ok() {}
                    let idle = now.saturating_duration_since(idle_start);
                    if idle >= IDLE_RESET {
                        rate_bps = INIT_RATE;
                        clients.clear();
                        tick_bytes = 0;
                        tick_pkts = 0;
                        tick_start = now;
                    } else {
                        tick_start += idle;
                    }
                    wait_for = wait_for.max(now);
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

        wait_for = wait_for.max(Instant::now());

        let chunks_addr = SocketAddrV4::new(chunks_broadcast, CHUNKS_PORT);

        let mut encoder = Encoder::new(cdata);
        write_buf[4..CHUNK_PACKET_PREFIX_LEN].copy_from_slice(&index);
        while let Some(len) = encoder.next_packet(&mut write_buf[CHUNK_PACKET_PREFIX_LEN..]) {
            let mut now = Instant::now();
            if wait_for > now {
                if wait_for - now >= MIN_SLEEP {
                    delay.reset(wait_for);
                    tokio::select! {
                        res = &mut delay => res?,
                        _ = state.cancel_token.cancelled() => return Ok(()),
                    }
                    now = Instant::now();
                }
            } else if now - wait_for > MAX_BURST_LAG {
                wait_for = now - MAX_BURST_LAG;
            }

            write_buf[..4].copy_from_slice(&seq.to_le_bytes());
            seq = seq.wrapping_add(1);

            let total_len = CHUNK_PACKET_PREFIX_LEN + len;
            let sent_len = match socket.send_to(&write_buf[..total_len], chunks_addr).await {
                Ok(sent_len) => sent_len,
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.raw_os_error() == Some(105) => {
                    log::warn!("UDP broadcast send congestion on {chunks_broadcast}: {e}");
                    rate_bps = (rate_bps * 0.7).max(MIN_RATE);
                    delay.reset(Instant::now() + Duration::from_millis(1));
                    tokio::select! {
                        res = &mut delay => res?,
                        _ = state.cancel_token.cancelled() => return Ok(()),
                    }
                    wait_for = Instant::now();
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            ensure!(sent_len == total_len, "Could not send packet");

            wait_for += Duration::from_secs_f64(sent_len as f64 / rate_bps);
            tick_bytes += sent_len as u64;
            tick_pkts += 1;

            if now >= tick_start + TICK {
                while let Ok((peer, received, lost)) = feedback_rx.try_recv() {
                    let entry = clients.entry(peer).or_default();
                    entry.0 += received as u64;
                    entry.1 += lost as u64;
                }
                let mut losses: Vec<f64> = clients
                    .drain()
                    .filter(|&(_, (r, l))| r + l > 0)
                    .map(|(_, (r, l))| l as f64 / (r + l) as f64)
                    .collect();
                let elapsed = now.saturating_duration_since(tick_start);
                if !losses.is_empty() && tick_pkts >= 16 && !elapsed.is_zero() {
                    losses.sort_by(f64::total_cmp);
                    let loss = losses[(losses.len() - 1).saturating_sub(losses.len() / 10)];
                    let achieved = tick_bytes as f64 / elapsed.as_secs_f64();
                    let factor = if loss <= 0.001 && achieved >= rate_bps * 0.7 {
                        1.25
                    } else if loss <= 0.001 {
                        1.0
                    } else if loss <= 0.005 {
                        0.95
                    } else {
                        ((1.0 - loss) * 0.85).clamp(0.25, 0.8)
                    };
                    let new_rate = (rate_bps * factor).clamp(MIN_RATE, MAX_RATE);
                    if (new_rate - rate_bps).abs() >= 1.0 {
                        rate_bps = new_rate;
                        log::debug!(
                            "Broadcast speed for {chunks_broadcast}: {:.1} MB/s (loss {:.2}%)",
                            rate_bps / 1_000_000.0,
                            loss * 100.0
                        );
                    }
                }
                tick_start = now;
                tick_bytes = 0;
                tick_pkts = 0;
            }
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

    if last.row == 0
        && let Some(&(r, c)) = positions.iter().max()
    {
        last.row = r;
        last.col = c;
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

struct NetChannels {
    net: Ipv4Net,
    chunk_tx: Sender<ChunkHash>,
    feedback_tx: Sender<(Ipv4Addr, u32, u32)>,
}

async fn handle_requests(
    state: &State,
    socket: &UdpSocket,
    net_tx: Vec<NetChannels>,
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
                socket.send_to(&[], peer_addr).await?;
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
                let Some(entry) = net_tx.iter().find(|c| c.net.contains(&peer_ip)) else {
                    continue;
                };
                for hash in chunks {
                    entry.chunk_tx.send(hash).await?;
                }
            }
            Ok(UdpRequest::BroadcastFeedback { received, lost }) => {
                let IpAddr::V4(peer_ip) = peer_addr.ip() else {
                    continue;
                };
                let Some(entry) = net_tx.iter().find(|c| c.net.contains(&peer_ip)) else {
                    continue;
                };
                let _ = entry.feedback_tx.try_send((peer_ip, received, lost));
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
        let (tx, rx) = mpsc::channel(128);
        let (feedback_tx, feedback_rx) = mpsc::channel(1024);
        net_tx.push(NetChannels {
            net: pixie_net,
            chunk_tx: tx,
            feedback_tx,
        });
        net_rx.push((pixie_net.broadcast(), rx, feedback_rx));
    }

    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, ACTION_PORT)).await?;
    log::info!("Listening on {}", socket.local_addr()?);
    socket.set_broadcast(true)?;

    let mut tasks = vec![handle_requests(&state, &socket, net_tx).boxed()];

    for (broadcast_ip, rx, feedback_rx) in net_rx {
        tasks.push(broadcast_chunks(&state, &socket, broadcast_ip, rx, feedback_rx).boxed());
        tasks.push(broadcast_hint(&state, &socket, broadcast_ip).boxed());
    }

    futures::future::try_join_all(tasks).await?;

    Ok(())
}
