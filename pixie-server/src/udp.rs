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

struct ClientFeedback {
    peer: Ipv4Addr,
    received: u32,
    lost: u32,
    queue_delay_us: u32,
}

struct RateController {
    rate_bytes_s: f64,
    ceiling_bytes_s: Option<f64>,
    clean_ticks: u32,
    cooldown_until: Instant,
    tick_start: Instant,
    tick_bytes: u64,
    tick_pkts: u64,
    clients: HashMap<Ipv4Addr, (u64, u64, u32)>,
}

impl RateController {
    const MIN_RATE: f64 = 500_000.0; // 4 Mbps
    const INIT_RATE: f64 = 10_000_000.0; // 80 Mbps
    const MAX_RATE: f64 = 125_000_000.0; // 1 Gbps
    const TICK: Duration = Duration::from_millis(200);
    const CUT_COOLDOWN: Duration = Duration::from_millis(500);
    const IDLE_RESET: Duration = Duration::from_secs(10);
    const LOSS_CUT: f64 = 0.02;
    const LOSS_GROW: f64 = 0.005;
    const QDELAY_CUT_US: u32 = 3_000;
    const QDELAY_GROW_US: u32 = 2_000;

    fn new(now: Instant) -> Self {
        Self {
            rate_bytes_s: Self::INIT_RATE,
            ceiling_bytes_s: None,
            clean_ticks: 0,
            cooldown_until: now,
            tick_start: now,
            tick_bytes: 0,
            tick_pkts: 0,
            clients: HashMap::new(),
        }
    }

    fn packet_delay(&self, sent_len: usize) -> Duration {
        Duration::from_secs_f64(sent_len as f64 / self.rate_bytes_s)
    }

    fn on_idle(
        &mut self,
        now: Instant,
        idle: Duration,
        feedback_rx: &mut Receiver<ClientFeedback>,
    ) {
        while feedback_rx.try_recv().is_ok() {}
        if idle >= Self::IDLE_RESET {
            self.rate_bytes_s = Self::INIT_RATE;
            self.ceiling_bytes_s = None;
            self.clean_ticks = 0;
            self.cooldown_until = now;
            self.clients.clear();
            self.tick_bytes = 0;
            self.tick_pkts = 0;
            self.tick_start = now;
        } else {
            self.tick_start += idle;
        }
    }

    fn cut(&mut self, now: Instant, factor: f64) -> bool {
        if now < self.cooldown_until {
            return false;
        }
        self.ceiling_bytes_s = Some(match self.ceiling_bytes_s {
            Some(c) => 0.5 * c + 0.5 * self.rate_bytes_s,
            None => self.rate_bytes_s,
        });
        self.clean_ticks = 0;
        self.cooldown_until = now + Self::CUT_COOLDOWN;
        let new_rate = (self.rate_bytes_s * factor).clamp(Self::MIN_RATE, Self::MAX_RATE);
        let changed = (new_rate - self.rate_bytes_s).abs() >= 1.0;
        self.rate_bytes_s = new_rate;
        changed
    }

    fn on_send_congestion(&mut self, now: Instant) {
        self.cut(now, 0.85);
    }

    fn update_rate(
        &mut self,
        now: Instant,
        loss: f64,
        qdelay_us: u32,
        achieved_bytes_s: f64,
    ) -> bool {
        if loss > Self::LOSS_CUT || qdelay_us > Self::QDELAY_CUT_US {
            return self.cut(now, 0.88);
        }
        if loss <= Self::LOSS_GROW
            && qdelay_us <= Self::QDELAY_GROW_US
            && achieved_bytes_s >= self.rate_bytes_s * 0.7
        {
            self.clean_ticks += 1;
            if let Some(c) = self.ceiling_bytes_s
                && self.rate_bytes_s >= c * 1.35
            {
                self.ceiling_bytes_s = None;
            }
            let factor = match self.ceiling_bytes_s {
                Some(c) if self.rate_bytes_s >= c * 0.85 => {
                    if self.clean_ticks >= 2 {
                        self.clean_ticks = 0;
                        1.03
                    } else {
                        1.0
                    }
                }
                _ => 1.15,
            };
            let new_rate = (self.rate_bytes_s * factor).clamp(Self::MIN_RATE, Self::MAX_RATE);
            let changed = (new_rate - self.rate_bytes_s).abs() >= 1.0;
            self.rate_bytes_s = new_rate;
            return changed;
        }
        false
    }

    fn percentile_90<T: Copy>(vals: &mut [T], cmp: impl Fn(&T, &T) -> std::cmp::Ordering) -> T {
        vals.sort_by(cmp);
        vals[(vals.len() - 1).saturating_sub(vals.len() / 10)]
    }

    fn on_packet_sent(
        &mut self,
        now: Instant,
        sent_len: usize,
        feedback_rx: &mut Receiver<ClientFeedback>,
        broadcast_ip: Ipv4Addr,
    ) {
        self.tick_bytes += sent_len as u64;
        self.tick_pkts += 1;
        if now < self.tick_start + Self::TICK {
            return;
        }

        while let Ok(fb) = feedback_rx.try_recv() {
            let entry = self.clients.entry(fb.peer).or_default();
            entry.0 += fb.received as u64;
            entry.1 += fb.lost as u64;
            entry.2 = entry.2.max(fb.queue_delay_us);
        }

        let mut losses = Vec::new();
        let mut qdelays = Vec::new();
        for (_, (r, l, qd)) in self.clients.drain() {
            if r + l > 0 {
                losses.push(l as f64 / (r + l) as f64);
                qdelays.push(qd);
            }
        }

        let elapsed = now.saturating_duration_since(self.tick_start);
        if !losses.is_empty() && self.tick_pkts >= 16 && !elapsed.is_zero() {
            let loss = Self::percentile_90(&mut losses, f64::total_cmp);
            let qdelay_us = Self::percentile_90(&mut qdelays, Ord::cmp);
            let achieved = self.tick_bytes as f64 / elapsed.as_secs_f64();
            if self.update_rate(now, loss, qdelay_us, achieved) {
                log::debug!(
                    "Broadcast speed for {broadcast_ip}: {:.1} MB/s (loss {:.2}%, qdelay {:.1}ms)",
                    self.rate_bytes_s / 1_000_000.0,
                    loss * 100.0,
                    qdelay_us as f64 / 1000.0,
                );
            }
        }

        self.tick_start = now;
        self.tick_bytes = 0;
        self.tick_pkts = 0;
    }
}

async fn broadcast_chunks(
    state: &State,
    socket: &UdpSocket,
    chunks_broadcast: Ipv4Addr,
    mut rx: Receiver<ChunkHash>,
    mut feedback_rx: Receiver<ClientFeedback>,
) -> Result<()> {
    const MIN_SLEEP: Duration = Duration::from_micros(100);
    const MAX_BURST_LAG: Duration = Duration::from_micros(250);

    let mut queue = BTreeSet::<ChunkHash>::new();
    let mut write_buf = [0; UDP_BODY_LEN];
    let start_time = Instant::now();
    let mut wait_for = start_time;
    let mut delay = tokio_timerfd::Delay::new(wait_for)?;
    let mut index = [0; 32];
    let mut seq: u32 = 0;
    let mut rate = RateController::new(start_time);

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
                    let idle = now.saturating_duration_since(idle_start);
                    rate.on_idle(now, idle, &mut feedback_rx);
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
        write_buf[8..CHUNK_PACKET_PREFIX_LEN].copy_from_slice(&index);
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
            let tx_us = start_time.elapsed().as_micros() as u32;
            write_buf[4..8].copy_from_slice(&tx_us.to_le_bytes());
            seq = seq.wrapping_add(1);

            let total_len = CHUNK_PACKET_PREFIX_LEN + len;
            let sent_len = match socket.send_to(&write_buf[..total_len], chunks_addr).await {
                Ok(sent_len) => sent_len,
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.raw_os_error() == Some(105) => {
                    log::warn!("UDP broadcast send congestion on {chunks_broadcast}: {e}");
                    let now = Instant::now();
                    rate.on_send_congestion(now);
                    delay.reset(now + Duration::from_millis(1));
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

            wait_for += rate.packet_delay(sent_len);
            rate.on_packet_sent(now, sent_len, &mut feedback_rx, chunks_broadcast);
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
    feedback_tx: Sender<ClientFeedback>,
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
            Ok(UdpRequest::BroadcastFeedback {
                received,
                lost,
                queue_delay_us,
            }) => {
                let IpAddr::V4(peer_ip) = peer_addr.ip() else {
                    continue;
                };
                let Some(entry) = net_tx.iter().find(|c| c.net.contains(&peer_ip)) else {
                    continue;
                };
                let _ = entry.feedback_tx.try_send(ClientFeedback {
                    peer: peer_ip,
                    received,
                    lost,
                    queue_delay_us,
                });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_controller_ramp_cut_cooldown_and_idle_reset() {
        let t0 = Instant::now();
        let mut rc = RateController::new(t0);
        let init = rc.rate_bytes_s;

        // Clean tick grows by 15% when no ceiling is set.
        assert!(rc.update_rate(t0 + Duration::from_millis(200), 0.0, 0, init));
        let r1 = rc.rate_bytes_s;
        assert!((r1 - init * 1.15).abs() < 1e-3);

        // Loss above threshold cuts by 0.88x and sets ceiling; second cut inside cooldown is skipped.
        let cut_time = t0 + Duration::from_millis(600);
        assert!(rc.update_rate(cut_time, 0.03, 0, r1));
        let after_cut = rc.rate_bytes_s;
        assert!((after_cut - r1 * 0.88).abs() < 1e-3);
        assert_eq!(rc.ceiling_bytes_s, Some(r1));
        assert!(!rc.update_rate(cut_time + Duration::from_millis(200), 0.0, 5_000, after_cut));
        assert_eq!(rc.rate_bytes_s, after_cut);

        // Above 0.85 * ceiling: first clean tick holds, second grows by 1.03x.
        assert!(!rc.update_rate(cut_time + Duration::from_millis(400), 0.0, 0, after_cut));
        assert!(rc.update_rate(cut_time + Duration::from_millis(600), 0.0, 0, after_cut));
        assert!((rc.rate_bytes_s - after_cut * 1.03).abs() < 1e-3);

        // Idle >= IDLE_RESET resets back to INIT_RATE and clears ceiling.
        let (_tx, mut rx) = mpsc::channel(4);
        rc.on_idle(cut_time + Duration::from_secs(12), Duration::from_secs(10), &mut rx);
        assert_eq!(rc.rate_bytes_s, RateController::INIT_RATE);
        assert_eq!(rc.ceiling_bytes_s, None);
    }
}
