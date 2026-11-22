use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::fmt::Write;
use core::mem;
use core::net::SocketAddrV4;
use core::time::Duration;

use futures::future::{Either, select};
use log::info;
use lz4_flex::decompress;
use pixie_shared::chunk_codec::Decoder;
use pixie_shared::util::BytesFmt;
use pixie_shared::{
    CHUNK_PACKET_PREFIX_LEN, CHUNKS_PORT, ChunkHash, Image, MAX_CHUNK_SIZE, UdpRequest,
};

use crate::MIN_MEMORY;
use crate::os::boot_options::BootOptions;
use crate::os::error::{Error, Result};
use crate::os::executor::Executor;
use crate::os::net::UdpSocket;
use crate::os::timer::Timer;
use crate::os::ui::{DrawArea, update_content};
use crate::os::{disk, memory};
use crate::rpc::RpcClient;

#[derive(Clone, PartialEq, Eq)]
struct Stats {
    chunks: usize,
    unique: usize,
    fetch: usize,
    recv: usize,
    pack_recv: usize,
    requested: usize,
}

fn handle_packet(
    buf: &[u8],
    chunks_info: &mut BTreeMap<ChunkHash, (usize, usize, Vec<usize>)>,
    received: &mut BTreeMap<ChunkHash, Decoder>,
    last_seen: &mut Vec<ChunkHash>,
) -> Result<Option<(Vec<usize>, Vec<u8>)>> {
    let hash: ChunkHash = buf[..32].try_into().unwrap();
    let csize = match chunks_info.get(&hash) {
        Some(&(_, csize, _)) => csize,
        _ => return Ok(None),
    };

    let decoder = received.entry(hash).or_insert_with(|| Decoder::new(csize));
    last_seen.retain(|x| x != &hash);
    last_seen.push(hash);

    if let Err(e) = decoder.add_packet(&buf[32..]) {
        log::warn!("Received invalid packet for chunk {hash:02x?}: {e}");
        return Ok(None);
    }
    let Some(cdata) = decoder.finish() else {
        return Ok(None);
    };

    let (size, _, pos) = chunks_info.remove(&hash).unwrap();
    received.remove(&hash).unwrap();
    last_seen.retain(|x| x != &hash);

    let data = decompress(&cdata, size)?;
    assert_eq!(data.len(), size);

    Ok(Some((pos, data)))
}

const BASE_OWD_WINDOWS: usize = 5;

struct FeedbackState {
    received: u32,
    lost: u32,
    window_min_owd: Option<u32>,
    base_history: [Option<u32>; BASE_OWD_WINDOWS],
    base_idx: usize,
    last_fb: i64,
}

impl FeedbackState {
    fn new() -> Self {
        Self {
            received: 0,
            lost: 0,
            window_min_owd: None,
            base_history: [None; BASE_OWD_WINDOWS],
            base_idx: 0,
            last_fb: Timer::micros(),
        }
    }

    fn record_packet(&mut self, tx_us: u32) {
        let rx_us = Timer::micros() as u32;
        let owd = rx_us.wrapping_sub(tx_us);
        self.window_min_owd = Some(match self.window_min_owd {
            Some(m) if (owd.wrapping_sub(m) as i32) >= 0 => m,
            _ => owd,
        });
    }

    async fn send(&mut self, socket: &UdpSocket, server_addr: SocketAddrV4) -> Result<()> {
        self.last_fb = Timer::micros();
        let queue_delay_us = if let Some(cur_min) = self.window_min_owd.take() {
            self.base_history[self.base_idx] = Some(cur_min);
            self.base_idx = (self.base_idx + 1) % BASE_OWD_WINDOWS;
            let mut base = cur_min;
            for m in self.base_history.into_iter().flatten() {
                if (m.wrapping_sub(base) as i32) < 0 {
                    base = m;
                }
            }
            cur_min.wrapping_sub(base)
        } else {
            0
        };

        if self.received > 0 || self.lost > 0 {
            let msg = postcard::to_allocvec(&UdpRequest::BroadcastFeedback {
                received: mem::take(&mut self.received),
                lost: mem::take(&mut self.lost),
                queue_delay_us,
            })?;
            socket.send_to(server_addr, &msg).await?;
        }
        Ok(())
    }
}

async fn fetch_image(server_addr: SocketAddrV4, hash: ChunkHash, len: usize) -> Result<Image> {
    let socket = UdpSocket::bind(Some(CHUNKS_PORT)).await?;
    let req = postcard::to_allocvec(&UdpRequest::RequestChunks(vec![hash]))?;
    let mut decoder = Decoder::new(len);

    socket.send_to(server_addr, &req).await?;
    loop {
        let recv = Box::pin(socket.recv_with(|pkt, _addr| {
            if pkt.len() < CHUNK_PACKET_PREFIX_LEN + 2 || pkt[8..CHUNK_PACKET_PREFIX_LEN] != hash {
                return None;
            }
            if let Err(e) = decoder.add_packet(&pkt[CHUNK_PACKET_PREFIX_LEN..]) {
                log::warn!("Received invalid packet for image {hash:02x?}: {e}");
                return None;
            }
            decoder.finish()
        }));
        let sleep = Box::pin(Executor::sleep(Duration::from_millis(100)));
        let res = match select(recv, sleep).await {
            Either::Left((bytes, _)) => bytes,
            Either::Right(((), _)) => {
                socket.send_to(server_addr, &req).await?;
                None
            }
        };
        if let Some(bytes) = res {
            if blake3::hash(&bytes).as_bytes() != &hash {
                return Err(Error::msg("Image hash mismatch"));
            }
            return Ok(postcard::from_bytes(&bytes)?);
        }
    }
}

pub async fn flash(server_addr: SocketAddrV4) -> Result<()> {
    let (image_hash, image_len) = RpcClient::new(*server_addr.ip())
        .await?
        .get_image_info()
        .await?;
    let image = fetch_image(server_addr, image_hash, image_len).await?;

    let mut chunks_info = BTreeMap::new();
    for chunk in &image.disk {
        chunks_info
            .entry(chunk.hash)
            .or_insert((chunk.size, chunk.csize, Vec::new()))
            .2
            .push(chunk.start);
    }

    info!("Obtained chunks; {} distinct chunks", chunks_info.len());

    let stats = RefCell::new(Stats {
        chunks: image.disk.len(),
        unique: chunks_info.len(),
        fetch: 0,
        recv: 0,
        pack_recv: 0,
        requested: 0,
    });

    let draw = |draw_area: &mut DrawArea| {
        draw_area.clear();
        writeln!(draw_area, "{} total chunks", stats.borrow().chunks).unwrap();
        writeln!(draw_area, "{} unique chunks", stats.borrow().unique).unwrap();
        writeln!(draw_area, "{} chunks to fetch", stats.borrow().fetch).unwrap();
        writeln!(draw_area, "{} chunks received", stats.borrow().recv).unwrap();
        writeln!(draw_area, "{} packets received", stats.borrow().pack_recv).unwrap();
        writeln!(draw_area, "{} chunks requested", stats.borrow().requested).unwrap();
    };

    update_content(draw);

    let done = Cell::new(false);

    let draw_task = async {
        let mut last_stats = stats.borrow().clone();
        while !done.get() {
            Executor::sleep(Duration::from_millis(100)).await;
            let stats = stats.borrow().clone();
            if stats == last_stats {
                continue;
            }
            update_content(draw);
            last_stats = stats;
        }
        Ok(())
    };

    let mut disk = disk::Disk::largest();

    for (hash, (size, csize, pos)) in mem::take(&mut chunks_info) {
        let mut found = None;
        let mut buf = vec![0; size];
        for &offset in &pos {
            disk.read(offset as u64, &mut buf).await.unwrap();
            if blake3::hash(&buf).as_bytes() == &hash {
                found = Some(offset);
                break;
            }
        }
        if let Some(found) = found {
            for &offset in &pos {
                if offset != found {
                    disk.write(offset as u64, &buf).await.unwrap();
                }
            }
        } else {
            chunks_info.insert(hash, (size, csize, pos));
            stats.borrow_mut().fetch = chunks_info.len();
        }
        update_content(draw);
    }

    info!("Disk scanned; {} chunks to fetch", stats.borrow().fetch);

    let socket = UdpSocket::bind(Some(CHUNKS_PORT)).await?;

    let mut received = BTreeMap::new();

    let (tx, rx) = thingbuf::mpsc::channel(128);

    let task1 = async {
        let tx = tx;
        let mut last_seen = Vec::new();
        let mut last_seq = None::<u32>;
        let mut fb = FeedbackState::new();
        let free_mem = memory::stats().free;
        let max_chunks = (free_mem.saturating_sub(MIN_MEMORY) as usize / MAX_CHUNK_SIZE).max(128);
        log::debug!(
            "Free memory: {}. Max chunks in memory: {max_chunks}",
            BytesFmt(free_mem)
        );

        while !chunks_info.is_empty() {
            let recv = Box::pin(socket.recv_with(|pkt, _addr| {
                if pkt.len() < CHUNK_PACKET_PREFIX_LEN + 2 {
                    return Ok::<_, Error>(None);
                }
                stats.borrow_mut().pack_recv += 1;
                let seq = u32::from_le_bytes(pkt[..4].try_into().unwrap());
                let tx_us = u32::from_le_bytes(pkt[4..8].try_into().unwrap());
                fb.record_packet(tx_us);

                if let Some(prev) = last_seq {
                    let gap = seq.wrapping_sub(prev).wrapping_sub(1);
                    if gap < 100_000 {
                        fb.lost = fb.lost.saturating_add(gap);
                        last_seq = Some(seq);
                    } else if seq.wrapping_sub(prev) < u32::MAX - 64 {
                        last_seq = Some(seq);
                    }
                } else {
                    last_seq = Some(seq);
                }
                fb.received = fb.received.saturating_add(1);

                handle_packet(&pkt[8..], &mut chunks_info, &mut received, &mut last_seen)
            }));
            let sleep = Box::pin(Executor::sleep(Duration::from_millis(100)));
            let res = match select(recv, sleep).await {
                Either::Left((chunk_res, _)) => Some(chunk_res),
                Either::Right(((), _)) => None,
            };
            match res {
                Some(chunk_res) => {
                    if let Some((pos, data)) = chunk_res? {
                        tx.send((pos, data)).await.expect("receiver was dropped");
                    }

                    assert_eq!(last_seen.len(), received.len());
                    if last_seen.len() > max_chunks {
                        let hash = last_seen.remove(0);
                        received
                            .remove(&hash)
                            .expect("last_seen should contain only received chunks");
                    }

                    let now_us = Timer::micros();
                    let elapsed = now_us - fb.last_fb;
                    if elapsed >= 200_000 || (fb.lost >= 16 && elapsed >= 50_000) {
                        fb.send(&socket, server_addr).await?;
                    }
                }
                None => {
                    fb.send(&socket, server_addr).await?;

                    // TODO(virv): compute the number of chunks to request
                    let chunks: Vec<_> =
                        chunks_info.iter().take(40).map(|(hash, _)| *hash).collect();
                    stats.borrow_mut().requested += chunks.len();
                    let msg = postcard::to_allocvec(&UdpRequest::RequestChunks(chunks)).unwrap();
                    socket.send_to(server_addr, &msg).await?;
                }
            }
        }
        fb.send(&socket, server_addr).await?;
        Ok::<_, Error>(())
    };

    let task2 = async {
        while let Some((pos, data)) = rx.recv().await {
            for offset in pos {
                disk.write(offset as u64, &data).await?;
            }

            stats.borrow_mut().recv += 1;

            let msg = UdpRequest::ActionProgress(stats.borrow().recv, stats.borrow().fetch);
            socket
                .send_to(server_addr, &postcard::to_allocvec(&msg)?)
                .await?;
        }
        done.set(true);
        Ok(())
    };

    let ((), (), ()) = futures::try_join!(task1, task2, draw_task)?;

    info!("Fetch complete, updating boot options");

    let mut order = BootOptions::order();
    let reboot_target = BootOptions::reboot_target();
    if let Some(target) = reboot_target {
        order = order
            .into_iter()
            .map(|x| if x != target { x } else { image.boot_option_id })
            .collect();
    } else {
        order.push(image.boot_option_id);
    };
    BootOptions::set_order(&order);
    BootOptions::set(image.boot_option_id, &image.boot_entry);

    Ok(())
}
