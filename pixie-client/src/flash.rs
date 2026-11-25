use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::fmt::Write;
use core::future::poll_fn;
use core::mem;
use core::net::SocketAddrV4;
use core::ops::{Deref, DerefMut};
use core::task::{Poll, Waker};
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
use crate::os::executor::event::Event;
use crate::os::net::UdpSocket;
use crate::os::timer::Timer;
use crate::os::ui::{DrawArea, update_content};
use crate::os::{disk, memory};
use crate::rpc::RpcClient;
use crate::{build_contiguous_batches, hash_cooperative, is_all_zero, zero_hash};

#[derive(Clone, PartialEq, Eq)]
struct Stats {
    chunks: usize,
    unique: usize,
    fetch: usize,
    recv: usize,
    pack_recv: usize,
    requested: usize,
}

const BASE_OWD_WINDOWS: usize = 16;

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

struct DiskLock {
    disk: RefCell<Option<disk::Disk>>,
    waiters: RefCell<Vec<Waker>>,
}

impl DiskLock {
    fn new(disk: disk::Disk) -> Self {
        Self {
            disk: RefCell::new(Some(disk)),
            waiters: RefCell::new(Vec::new()),
        }
    }

    async fn lock(&self) -> DiskGuard<'_> {
        poll_fn(|cx| {
            if let Some(d) = self.disk.borrow_mut().take() {
                Poll::Ready(DiskGuard {
                    lock: self,
                    disk: Some(d),
                })
            } else {
                self.waiters.borrow_mut().push(cx.waker().clone());
                Poll::Pending
            }
        })
        .await
    }
}

struct DiskGuard<'a> {
    lock: &'a DiskLock,
    disk: Option<disk::Disk>,
}

impl Deref for DiskGuard<'_> {
    type Target = disk::Disk;

    fn deref(&self) -> &Self::Target {
        self.disk.as_ref().expect("DiskGuard missing Disk")
    }
}

impl DerefMut for DiskGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.disk.as_mut().expect("DiskGuard missing Disk")
    }
}

impl Drop for DiskGuard<'_> {
    fn drop(&mut self) {
        if let Some(d) = self.disk.take() {
            *self.lock.disk.borrow_mut() = Some(d);
            for w in self.lock.waiters.borrow_mut().drain(..) {
                w.wake();
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ChunkPhase {
    Scanning,
    NeedFetch,
    Resolved,
}

#[derive(Debug)]
struct ChunkState {
    size: usize,
    csize: usize,
    all_offsets: Vec<usize>,
    remaining_to_scan: usize,
    valid_source: Option<usize>,
    mismatched_offsets: Vec<usize>,
    phase: ChunkPhase,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum WriteJob {
    #[default]
    Empty,
    Data {
        offsets: Vec<usize>,
        data: Vec<u8>,
    },
    LocalRepair {
        src_offset: usize,
        size: usize,
        dst_offsets: Vec<usize>,
    },
    ZeroFill {
        size: usize,
        dst_offsets: Vec<usize>,
    },
}

pub struct FlashState {
    chunks: BTreeMap<ChunkHash, ChunkState>,
    need_fetch: BTreeSet<ChunkHash>,
    zero_hashes: BTreeMap<usize, ChunkHash>,
    unresolved_count: usize,
    scan_done: bool,
}

impl FlashState {
    fn new(image: &Image) -> (Self, Vec<(usize, ChunkHash, usize)>) {
        let mut chunks: BTreeMap<ChunkHash, ChunkState> = BTreeMap::new();
        let mut zero_hashes: BTreeMap<usize, ChunkHash> = BTreeMap::new();
        let mut occurrences = Vec::with_capacity(image.disk.len());

        for chunk in &image.disk {
            zero_hashes
                .entry(chunk.size)
                .or_insert_with(|| zero_hash(chunk.size));
            occurrences.push((chunk.start, chunk.hash, chunk.size));
            let entry = chunks.entry(chunk.hash).or_insert_with(|| ChunkState {
                size: chunk.size,
                csize: chunk.csize,
                all_offsets: Vec::new(),
                remaining_to_scan: 0,
                valid_source: None,
                mismatched_offsets: Vec::new(),
                phase: ChunkPhase::Scanning,
            });
            entry.all_offsets.push(chunk.start);
            entry.remaining_to_scan += 1;
        }
        occurrences.sort_unstable_by_key(|&(offset, _, _)| offset);

        let unresolved_count = chunks.len();
        (
            Self {
                chunks,
                need_fetch: BTreeSet::new(),
                zero_hashes,
                unresolved_count,
                scan_done: occurrences.is_empty(),
            },
            occurrences,
        )
    }

    fn is_resolved(&self, hash: &ChunkHash) -> bool {
        self.chunks
            .get(hash)
            .is_none_or(|st| st.phase == ChunkPhase::Resolved)
    }

    fn wants_packet(&self, hash: &ChunkHash) -> Option<(usize, usize)> {
        let st = self.chunks.get(hash)?;
        if st.phase != ChunkPhase::Resolved && st.valid_source.is_none() {
            Some((st.size, st.csize))
        } else {
            None
        }
    }

    fn zero_hash_for(&self, size: usize) -> &ChunkHash {
        self.zero_hashes
            .get(&size)
            .expect("precomputed zero hash missing")
    }

    fn on_scan_result(
        &mut self,
        offset: usize,
        hash: ChunkHash,
        matched: bool,
    ) -> (Option<WriteJob>, bool) {
        let Some(st) = self.chunks.get_mut(&hash) else {
            return (None, false);
        };
        if st.phase == ChunkPhase::Resolved {
            return (None, false);
        }

        st.remaining_to_scan -= 1;
        if matched {
            st.valid_source.get_or_insert(offset);
        } else {
            st.mismatched_offsets.push(offset);
        }

        if st.remaining_to_scan > 0 {
            return (None, false);
        }

        if st.mismatched_offsets.is_empty() {
            st.phase = ChunkPhase::Resolved;
            st.all_offsets = Vec::new();
            self.unresolved_count -= 1;
            return (None, false);
        }

        if let Some(src_offset) = st.valid_source {
            st.phase = ChunkPhase::Resolved;
            st.all_offsets = Vec::new();
            self.unresolved_count -= 1;
            return (
                Some(WriteJob::LocalRepair {
                    src_offset,
                    size: st.size,
                    dst_offsets: mem::take(&mut st.mismatched_offsets),
                }),
                false,
            );
        }

        if self.zero_hashes.get(&st.size) == Some(&hash) {
            st.phase = ChunkPhase::Resolved;
            st.all_offsets = Vec::new();
            self.unresolved_count -= 1;
            return (
                Some(WriteJob::ZeroFill {
                    size: st.size,
                    dst_offsets: mem::take(&mut st.mismatched_offsets),
                }),
                false,
            );
        }

        st.phase = ChunkPhase::NeedFetch;
        self.need_fetch.insert(hash);
        (None, true)
    }

    fn on_net_chunk(&mut self, hash: ChunkHash, data: Vec<u8>) -> (Option<WriteJob>, bool) {
        let Some(st) = self.chunks.get_mut(&hash) else {
            return (None, false);
        };
        if st.phase == ChunkPhase::Resolved || st.valid_source.is_some() {
            return (None, false);
        }
        match st.phase {
            ChunkPhase::NeedFetch => {
                self.need_fetch.remove(&hash);
                st.phase = ChunkPhase::Resolved;
                st.all_offsets = Vec::new();
                self.unresolved_count -= 1;
                (
                    Some(WriteJob::Data {
                        offsets: mem::take(&mut st.mismatched_offsets),
                        data,
                    }),
                    false,
                )
            }
            ChunkPhase::Scanning => {
                st.phase = ChunkPhase::Resolved;
                st.mismatched_offsets = Vec::new();
                self.unresolved_count -= 1;
                (
                    Some(WriteJob::Data {
                        offsets: mem::take(&mut st.all_offsets),
                        data,
                    }),
                    true,
                )
            }
            ChunkPhase::Resolved => (None, false),
        }
    }

    fn is_all_done(&self) -> bool {
        self.scan_done && self.unresolved_count == 0
    }
}

pub fn feed_packet(
    buf: &[u8],
    state: &FlashState,
    received: &mut BTreeMap<ChunkHash, Decoder>,
    last_seen: &mut Vec<ChunkHash>,
) -> Option<(ChunkHash, usize, Vec<u8>)> {
    if buf.len() < 34 {
        return None;
    }
    let hash: ChunkHash = buf[..32].try_into().unwrap();
    let (size, csize) = state.wants_packet(&hash)?;

    let decoder = received.entry(hash).or_insert_with(|| Decoder::new(csize));
    if last_seen.last() != Some(&hash) {
        last_seen.retain(|x| x != &hash);
        last_seen.push(hash);
    }

    if let Err(e) = decoder.add_packet(&buf[32..]) {
        log::warn!("Received invalid packet for chunk {hash:02x?}: {e}");
        return None;
    }
    let cdata = decoder.finish()?;

    received.remove(&hash);
    last_seen.retain(|x| x != &hash);

    Some((hash, size, cdata))
}

async fn check_chunk_cooperative(data: &[u8], expected: &ChunkHash, zhash: &ChunkHash) -> bool {
    if is_all_zero(data) {
        expected == zhash
    } else if expected == zhash {
        false
    } else {
        hash_cooperative(data).await == *expected
    }
}

enum NetTick {
    Packet(Result<Option<(ChunkHash, usize, Vec<u8>)>>),
    RequestNow,
    Done,
}

pub async fn flash(server_addr: SocketAddrV4) -> Result<()> {
    let (image_hash, image_len) = RpcClient::new(*server_addr.ip())
        .await?
        .get_image_info()
        .await?;
    let image = fetch_image(server_addr, image_hash, image_len).await?;

    let (initial_state, occurrences) = FlashState::new(&image);
    let total_unique = initial_state.chunks.len();

    info!("Obtained chunks; {} distinct chunks", total_unique);

    let stats = RefCell::new(Stats {
        chunks: image.disk.len(),
        unique: total_unique,
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

    const SCAN_BATCH_BYTES: usize = 2 * MAX_CHUNK_SIZE; // 8 MiB per NVMe read-ahead batch
    const SCAN_QUEUE_DEPTH: usize = 4; // 32 MiB bounded read-ahead pool
    const ZERO_YIELD_SLICE: usize = 512 * 1024;

    let disk_lock = DiskLock::new(disk::Disk::largest().await);
    let socket = UdpSocket::bind(Some(CHUNKS_PORT)).await?;

    let free_mem = memory::stats().free;
    let usable_mem =
        free_mem.saturating_sub(MIN_MEMORY + (SCAN_QUEUE_DEPTH * SCAN_BATCH_BYTES) as u64) as usize;
    let channel_size = (usable_mem / (4 * MAX_CHUNK_SIZE)).clamp(32, 128);
    let max_chunks = (usable_mem / (2 * MAX_CHUNK_SIZE)).clamp(64, 128);
    log::debug!(
        "Free memory: {}. Channel size: {channel_size}, max chunks in memory: {max_chunks}",
        BytesFmt(free_mem)
    );

    let state = RefCell::new(initial_state);
    let batches = build_contiguous_batches(&occurrences, SCAN_BATCH_BYTES, |&(o, _, s)| (o, s));

    let (scan_free_tx, scan_free_rx) = thingbuf::mpsc::channel::<Vec<u8>>(SCAN_QUEUE_DEPTH);
    for _ in 0..SCAN_QUEUE_DEPTH {
        scan_free_tx
            .try_send(vec![0u8; SCAN_BATCH_BYTES])
            .expect("pre-seeding scan read buffers");
    }
    let (scan_filled_tx, scan_filled_rx) =
        thingbuf::mpsc::channel::<(usize, Vec<u8>)>(SCAN_QUEUE_DEPTH);
    let (write_tx, write_rx) = thingbuf::mpsc::channel::<WriteJob>(channel_size);
    let write_tx_scan = write_tx.clone();

    let done = Cell::new(false);
    let mut first_req_event = Event::new();
    let first_req_trigger = first_req_event.trigger();
    let mut done_event = Event::new();
    let done_trigger = done_event.trigger();

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
        Ok::<_, Error>(())
    };

    let scan_reader_task = async {
        let scan_filled_tx = scan_filled_tx;
        let scan_free_rx = scan_free_rx;
        for (b_idx, &(b_off, b_len, ref b_range)) in batches.iter().enumerate() {
            let all_resolved = {
                let st = state.borrow();
                occurrences[b_range.clone()]
                    .iter()
                    .all(|(_, h, _)| st.is_resolved(h))
            };
            if all_resolved {
                continue;
            }
            let Some(mut buf) = scan_free_rx.recv().await else {
                break;
            };
            if b_len > buf.len() {
                buf.resize(b_len, 0);
            }
            {
                let mut disk = disk_lock.lock().await;
                disk.read(b_off, &mut buf[..b_len]).await?;
            }
            if scan_filled_tx.send((b_idx, buf)).await.is_err() {
                break;
            }
        }
        Ok::<_, Error>(())
    };

    let scan_hasher_task = async {
        let write_tx = write_tx_scan;
        let scan_free_tx = scan_free_tx;
        let mut zero_bytes_since_yield = 0usize;

        while let Some((b_idx, buf)) = scan_filled_rx.recv().await {
            let (b_off, _, ref b_range) = batches[b_idx];
            for &(offset, hash, size) in &occurrences[b_range.clone()] {
                if state.borrow().is_resolved(&hash) {
                    continue;
                }
                let rel = offset - b_off as usize;
                let data = &buf[rel..rel + size];
                let zhash = *state.borrow().zero_hash_for(size);
                let matched = check_chunk_cooperative(data, &hash, &zhash).await;
                if is_all_zero(data) || hash == zhash {
                    zero_bytes_since_yield += size;
                    if zero_bytes_since_yield >= ZERO_YIELD_SLICE {
                        zero_bytes_since_yield = 0;
                        Executor::sched_yield().await;
                    }
                } else {
                    zero_bytes_since_yield = 0;
                }

                let (job, newly_need_fetch, need_len) = {
                    let mut st = state.borrow_mut();
                    let (job, newly_need_fetch) = st.on_scan_result(offset, hash, matched);
                    (job, newly_need_fetch, st.need_fetch.len())
                };
                if newly_need_fetch {
                    stats.borrow_mut().fetch += 1;
                    if need_len >= 40 {
                        first_req_trigger.trigger();
                    }
                }
                if let Some(job) = job {
                    write_tx.send(job).await.expect("writer was dropped");
                }
            }
            let _ = scan_free_tx.send(buf).await;
        }

        // Release all 32 MiB of recycled scan buffers as soon as hashing completes.
        drop(scan_free_tx);

        let all_done = {
            let mut st = state.borrow_mut();
            st.scan_done = true;
            if !st.need_fetch.is_empty() {
                first_req_trigger.trigger();
            }
            st.is_all_done()
        };
        if all_done {
            done_trigger.trigger();
        }
        update_content(draw);
        info!("Disk scanned; {} chunks to fetch", stats.borrow().fetch);
        Ok::<_, Error>(())
    };

    let net_recv_task = async {
        let write_tx = write_tx;
        let mut received = BTreeMap::new();
        let mut last_seen = Vec::new();
        let mut last_seq = None::<u32>;
        let mut fb = FeedbackState::new();
        let mut sent_initial_req = false;

        while !state.borrow().is_all_done() {
            let tick = {
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

                    let st = state.borrow();
                    Ok(feed_packet(&pkt[8..], &st, &mut received, &mut last_seen))
                }));
                let sleep = Box::pin(Executor::sleep(Duration::from_millis(100)));
                let net_or_sleep = select(recv, sleep);
                if !sent_initial_req {
                    let events = select(&mut first_req_event, &mut done_event);
                    match select(net_or_sleep, events).await {
                        Either::Left((Either::Left((chunk_res, _)), _)) => {
                            NetTick::Packet(chunk_res)
                        }
                        Either::Left((Either::Right(((), _)), _))
                        | Either::Right((Either::Left(((), _)), _)) => NetTick::RequestNow,
                        Either::Right((Either::Right(((), _)), _)) => NetTick::Done,
                    }
                } else {
                    match select(net_or_sleep, &mut done_event).await {
                        Either::Left((Either::Left((chunk_res, _)), _)) => {
                            NetTick::Packet(chunk_res)
                        }
                        Either::Left((Either::Right(((), _)), _)) => NetTick::RequestNow,
                        Either::Right(((), _)) => NetTick::Done,
                    }
                }
            };

            match tick {
                NetTick::Done => break,
                NetTick::Packet(chunk_res) => {
                    if let Some((hash, size, cdata)) = chunk_res? {
                        match decompress(&cdata, size) {
                            Ok(data)
                                if data.len() == size && hash_cooperative(&data).await == hash =>
                            {
                                let (job, was_early, all_done) = {
                                    let mut st = state.borrow_mut();
                                    let (job, was_early) = st.on_net_chunk(hash, data);
                                    (job, was_early, st.is_all_done())
                                };
                                if was_early {
                                    stats.borrow_mut().fetch += 1;
                                }
                                if let Some(job) = job {
                                    write_tx.send(job).await.expect("writer was dropped");
                                }
                                if all_done {
                                    done_trigger.trigger();
                                }
                            }
                            Ok(data) => {
                                log::warn!(
                                    "Decompressed chunk {hash:02x?} size/hash mismatch (got {} bytes, expected {size}); re-fetching",
                                    data.len()
                                );
                            }
                            Err(e) => {
                                log::warn!(
                                    "Failed to decompress chunk {hash:02x?}: {e}; re-fetching"
                                );
                            }
                        }
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
                    if elapsed >= 100_000 || (fb.lost >= 16 && elapsed >= 50_000) {
                        fb.send(&socket, server_addr).await?;
                    }
                }
                NetTick::RequestNow => {
                    fb.send(&socket, server_addr).await?;
                    let chunks: Vec<_> =
                        state.borrow().need_fetch.iter().copied().take(40).collect();
                    if !chunks.is_empty() {
                        sent_initial_req = true;
                        stats.borrow_mut().requested += chunks.len();
                        let msg = postcard::to_allocvec(&UdpRequest::RequestChunks(chunks))?;
                        socket.send_to(server_addr, &msg).await?;
                    }
                }
            }
        }
        fb.send(&socket, server_addr).await?;
        Ok::<_, Error>(())
    };

    let writer_task = async {
        let mut repair_buf = Vec::new();
        let mut zero_buf = Vec::new();
        let mut progress_buf = [0u8; 32];
        let mut last_progress_us = Timer::micros();

        while let Some(job) = write_rx.recv().await {
            match job {
                WriteJob::Empty => {}
                WriteJob::Data { offsets, data } => {
                    for offset in offsets {
                        let mut disk = disk_lock.lock().await;
                        disk.write(offset as u64, &data).await?;
                    }
                    let (recv, fetch) = {
                        let mut st = stats.borrow_mut();
                        st.recv += 1;
                        (st.recv, st.fetch)
                    };
                    let now_us = Timer::micros();
                    if now_us.saturating_sub(last_progress_us) >= 100_000 {
                        let pkt = postcard::to_slice(
                            &UdpRequest::ActionProgress(recv, fetch),
                            &mut progress_buf,
                        )?;
                        socket.send_to(server_addr, pkt).await?;
                        last_progress_us = now_us;
                    }
                }
                WriteJob::LocalRepair {
                    src_offset,
                    size,
                    dst_offsets,
                } => {
                    if size > repair_buf.len() {
                        repair_buf.resize(size, 0);
                    }
                    let buf = &mut repair_buf[..size];
                    {
                        let mut disk = disk_lock.lock().await;
                        disk.read(src_offset as u64, buf).await?;
                    }
                    for dst_offset in dst_offsets {
                        if dst_offset != src_offset {
                            let mut disk = disk_lock.lock().await;
                            disk.write(dst_offset as u64, buf).await?;
                        }
                    }
                }
                WriteJob::ZeroFill { size, dst_offsets } => {
                    if size > zero_buf.len() {
                        zero_buf.resize(size, 0);
                    }
                    let buf = &zero_buf[..size];
                    for dst_offset in dst_offsets {
                        let mut disk = disk_lock.lock().await;
                        disk.write(dst_offset as u64, buf).await?;
                    }
                }
            }
        }
        let (recv, fetch) = {
            let st = stats.borrow();
            (st.recv, st.fetch)
        };
        if fetch > 0 {
            let pkt =
                postcard::to_slice(&UdpRequest::ActionProgress(recv, fetch), &mut progress_buf)?;
            socket.send_to(server_addr, pkt).await?;
        }
        done.set(true);
        Ok::<_, Error>(())
    };

    let ((), (), (), (), ()) = futures::try_join!(
        scan_reader_task,
        scan_hasher_task,
        net_recv_task,
        writer_task,
        draw_task
    )?;

    update_content(draw);
    disk_lock.lock().await.flush().await?;

    info!("Fetch complete, updating boot options");

    BootOptions::set(image.boot_option_id, &image.boot_entry);

    let mut order = BootOptions::order();
    if let Some(target) = BootOptions::reboot_target() {
        let mut replaced = false;
        for x in &mut order {
            if *x == target {
                *x = image.boot_option_id;
                replaced = true;
            }
        }
        if !replaced && !order.contains(&image.boot_option_id) {
            order.push(image.boot_option_id);
        }
    } else if !order.contains(&image.boot_option_id) {
        order.push(image.boot_option_id);
    }
    let mut deduped = Vec::with_capacity(order.len());
    for id in order {
        if !deduped.contains(&id) {
            deduped.push(id);
        }
    }
    BootOptions::set_order(&deduped);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pixie_shared::Chunk;

    fn test_image(items: &[(ChunkHash, usize, usize)]) -> Image {
        Image {
            boot_option_id: 0,
            boot_entry: Vec::new(),
            disk: items
                .iter()
                .map(|&(hash, start, size)| Chunk {
                    hash,
                    start,
                    size,
                    csize: size / 2,
                })
                .collect(),
        }
    }

    #[test]
    fn test_flash_state_cached_cold_repair_zero_fill_and_early_broadcast() {
        let h_cached = [0x11; 32];
        let h_cold = [0x22; 32];
        let h_dup = [0x33; 32];
        let h_zero = zero_hash(4096);
        let h_early = [0x44; 32];

        let img = test_image(&[
            (h_cached, 0, 4096),
            (h_cold, 4096, 4096),
            (h_dup, 8192, 4096),
            (h_dup, 12288, 4096),
            (h_zero, 16384, 4096),
            (h_early, 20480, 4096),
        ]);
        let (mut st, occs) = FlashState::new(&img);
        assert_eq!(occs.len(), 6);
        assert!(!st.is_all_done());

        // Cached match resolves locally with no write and no fetch.
        assert_eq!(st.on_scan_result(0, h_cached, true), (None, false));
        assert!(st.is_resolved(&h_cached));
        assert!(st.wants_packet(&h_cached).is_none());

        // Cold mismatch queues NeedFetch and then resolves on network delivery.
        assert_eq!(st.on_scan_result(4096, h_cold, false), (None, true));
        assert!(st.need_fetch.contains(&h_cold));
        assert_eq!(
            st.on_net_chunk(h_cold, vec![9u8; 4096]),
            (
                Some(WriteJob::Data {
                    offsets: vec![4096],
                    data: vec![9u8; 4096],
                }),
                false
            )
        );
        assert!(st.is_resolved(&h_cold));
        assert!(!st.need_fetch.contains(&h_cold));

        // Duplicate: first occurrence mismatches, second matches -> LocalRepair, 0 fetch.
        assert_eq!(st.on_scan_result(8192, h_dup, false), (None, false));
        assert_eq!(
            st.on_scan_result(12288, h_dup, true),
            (
                Some(WriteJob::LocalRepair {
                    src_offset: 12288,
                    size: 4096,
                    dst_offsets: vec![8192],
                }),
                false
            )
        );
        assert!(st.is_resolved(&h_dup));

        // Dirty zero chunk repairs via ZeroFill without network fetch.
        assert_eq!(
            st.on_scan_result(16384, h_zero, false),
            (
                Some(WriteJob::ZeroFill {
                    size: 4096,
                    dst_offsets: vec![16384],
                }),
                false
            )
        );
        assert!(st.is_resolved(&h_zero));

        // Early broadcast arrives while Scanning -> resolved immediately, later scan ignored.
        assert!(st.wants_packet(&h_early).is_some());
        assert_eq!(
            st.on_net_chunk(h_early, vec![7u8; 4096]),
            (
                Some(WriteJob::Data {
                    offsets: vec![20480],
                    data: vec![7u8; 4096],
                }),
                true
            )
        );
        assert!(st.is_resolved(&h_early));
        assert_eq!(st.on_scan_result(20480, h_early, false), (None, false));

        st.scan_done = true;
        assert!(st.is_all_done());
    }
}
