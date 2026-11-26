use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::fmt::Write;
use core::net::SocketAddrV4;
use core::pin::pin;

use futures::future::{Either, select};
use log::info;
use lz4_flex::compress;
use pixie_shared::rpc::HAS_CHUNKS_BATCH_SIZE;
use pixie_shared::util::BytesFmt;
use pixie_shared::{Chunk, ChunkHash, Image, MAX_CHUNK_SIZE, Offset, UdpRequest};

use crate::os::boot_options::BootOptions;
use crate::os::error::{Error, Result};
use crate::os::executor::Executor;
use crate::os::net::UdpSocket;
use crate::os::timer::Timer;
use crate::os::ui::DrawArea;
use crate::os::{disk, memory, ui};
use crate::rpc::RpcClient;
use crate::{
    MIN_MEMORY, PageAlignedBuf, build_contiguous_batches, hash_cooperative, is_all_zero,
    parse_disk, zero_hash,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkInfo {
    pub start: Offset,
    pub size: usize,
}

struct PushingChunks {
    total: usize,
    read_chunks: usize,
    pushed_chunks: usize,
    tsize: usize,
    tcsize: usize,
}

enum State {
    ReadingPartitions,
    PushingChunks(PushingChunks),
}

impl State {
    fn as_pushing_chunks_mut(&mut self) -> &mut PushingChunks {
        match self {
            State::PushingChunks(pc) => pc,
            _ => panic!("Not in pushing chunks state"),
        }
    }
}

pub async fn store(server_address: SocketAddrV4) -> Result<()> {
    let stats = RefCell::new(State::ReadingPartitions);

    let draw = |draw_area: &mut DrawArea| {
        draw_area.clear();
        match &*stats.borrow() {
            State::ReadingPartitions => {
                writeln!(draw_area, "Reading partitions...").unwrap();
            }
            State::PushingChunks(PushingChunks {
                total,
                read_chunks,
                pushed_chunks,
                tsize,
                tcsize,
            }) => {
                writeln!(
                    draw_area,
                    "Pushed {pushed_chunks} / {read_chunks} / {total} chunks"
                )
                .unwrap();
                writeln!(draw_area, "total size {tsize}, compressed {tcsize}").unwrap();
            }
        }
    };

    ui::update_content(draw);

    let boid =
        BootOptions::reboot_target().ok_or_else(|| Error::msg("Could not find reboot target"))?;
    let bo_command = BootOptions::get(boid);

    let mut disk = disk::Disk::largest().await;
    let chunks = parse_disk::parse_disk(&mut disk).await?;
    info!(
        "Total size of chunks: {}",
        BytesFmt(chunks.iter().map(|x| x.size as u64).sum::<u64>())
    );

    let udp = UdpSocket::bind(None).await?;
    let rpc = RpcClient::new(*server_address.ip()).await?;
    let rpc_query = RpcClient::new(*server_address.ip()).await?;

    let total = chunks.len();

    let mut total_size = 0;
    let mut total_csize = 0;

    const READ_BATCH_BYTES: usize = 2 * MAX_CHUNK_SIZE; // 8 MiB per NVMe read-ahead batch
    const READ_QUEUE_DEPTH: usize = 4; // 32 MiB bounded read-ahead pool
    const ZERO_YIELD_SLICE: usize = 512 * 1024;

    let free_mem = memory::stats().free;
    let usable_mem =
        free_mem.saturating_sub(MIN_MEMORY + (READ_QUEUE_DEPTH * READ_BATCH_BYTES) as u64) as usize;
    let channel_size = (usable_mem / (4 * MAX_CHUNK_SIZE)).clamp(32, 128);
    log::debug!(
        "Free memory: {}. Channel size: {channel_size}",
        BytesFmt(free_mem)
    );

    stats.replace(State::PushingChunks(PushingChunks {
        total,
        read_chunks: 0,
        pushed_chunks: 0,
        tsize: 0,
        tcsize: 0,
    }));
    ui::update_content(draw);

    let batches = build_contiguous_batches(&chunks, READ_BATCH_BYTES, |c| (c.start, c.size));
    let (free_tx, free_rx) = thingbuf::mpsc::channel::<PageAlignedBuf>(READ_QUEUE_DEPTH);
    for _ in 0..READ_QUEUE_DEPTH {
        free_tx
            .try_send(PageAlignedBuf::zeroed(READ_BATCH_BYTES))
            .expect("pre-seeding free read buffers");
    }
    let (filled_tx, filled_rx) =
        thingbuf::mpsc::channel::<(usize, PageAlignedBuf)>(READ_QUEUE_DEPTH);

    let (tx, rx) = thingbuf::mpsc::channel::<(Chunk, Option<Vec<u8>>)>(channel_size);
    let last_draw_us = Cell::new(Timer::micros());
    let maybe_draw = || {
        let now = Timer::micros();
        if now.saturating_sub(last_draw_us.get()) >= 100_000 {
            ui::update_content(draw);
            last_draw_us.set(now);
        }
    };

    let reader_task = async {
        let filled_tx = filled_tx;
        let free_rx = free_rx;
        for (b_idx, &(b_off, b_len, _)) in batches.iter().enumerate() {
            let Some(mut buf) = free_rx.recv().await else {
                break;
            };
            buf.ensure_len(b_len);
            disk.read(b_off, &mut buf[..b_len]).await?;
            if filled_tx.send((b_idx, buf)).await.is_err() {
                break;
            }
        }
        Ok::<_, Error>(())
    };

    let hash_task = async {
        let tx = tx;
        let free_tx = free_tx;
        let mut seen_hashes: BTreeMap<ChunkHash, usize> = BTreeMap::new();
        let mut zero_hashes: BTreeMap<usize, ChunkHash> = BTreeMap::new();
        let mut zero_bytes_since_yield = 0usize;
        while let Some((b_idx, buf)) = filled_rx.recv().await {
            let (b_off, _, ref b_range) = batches[b_idx];
            for chunk_info in &chunks[b_range.clone()] {
                let rel = chunk_info.start - b_off as usize;
                let data = &buf[rel..rel + chunk_info.size];
                let hash: ChunkHash = if is_all_zero(data) {
                    zero_bytes_since_yield += data.len();
                    if zero_bytes_since_yield >= ZERO_YIELD_SLICE {
                        zero_bytes_since_yield = 0;
                        Executor::sched_yield().await;
                    }
                    *zero_hashes
                        .entry(data.len())
                        .or_insert_with(|| zero_hash(data.len()))
                } else {
                    zero_bytes_since_yield = 0;
                    hash_cooperative(data).await
                };
                let (csize, cdata) = if let Some(&csize) = seen_hashes.get(&hash) {
                    (csize, None)
                } else {
                    let cdata = compress(data);
                    if data.len() >= 256 * 1024 {
                        Executor::sched_yield().await;
                    }
                    let csize = cdata.len();
                    seen_hashes.insert(hash, csize);
                    (csize, Some(cdata))
                };
                let chunk = Chunk {
                    hash,
                    start: chunk_info.start,
                    size: chunk_info.size,
                    csize,
                };
                stats.borrow_mut().as_pushing_chunks_mut().read_chunks += 1;
                maybe_draw();
                tx.send((chunk, cdata)).await.expect("receiver dropped");
            }
            let _ = free_tx.send(buf).await;
        }
        Ok::<_, Error>(())
    };

    let upload_task = async {
        let mut uploaded_chunks = Vec::new();
        let mut batch = Vec::with_capacity(HAS_CHUNKS_BATCH_SIZE);
        let mut progress_buf = [0u8; 32];
        let mut last_progress_us = Timer::micros();
        let mut pipeline = rpc.blob_pipeline();
        loop {
            let first = loop {
                if pipeline.is_empty() {
                    break rx.recv().await;
                }
                match select(pin!(rx.recv()), pin!(pipeline.step())).await {
                    Either::Left((item, _)) => break item,
                    Either::Right((res, _)) => res?,
                }
            };
            let Some(first) = first else {
                break;
            };
            batch.clear();
            batch.push(first);
            while batch.len() < HAS_CHUNKS_BATCH_SIZE {
                let Ok(item) = rx.try_recv() else {
                    break;
                };
                batch.push(item);
            }

            let hashes: Vec<_> = batch
                .iter()
                .filter_map(|(c, cdata)| cdata.as_ref().map(|_| c.hash))
                .collect();
            let present = if hashes.is_empty() {
                Vec::new()
            } else {
                rpc_query.has_chunks(hashes).await?
            };

            let mut present_iter = present.into_iter();
            for (chunk, cdata) in batch.drain(..) {
                if let Some(cdata) = cdata {
                    let has_chunk = present_iter.next().unwrap_or(false);
                    if !has_chunk {
                        pipeline.push(cdata).await?;
                    }
                }
                uploaded_chunks.push(chunk);

                total_size += chunk.size;
                total_csize += chunk.csize;

                {
                    let mut st = stats.borrow_mut();
                    let pc = st.as_pushing_chunks_mut();
                    pc.pushed_chunks += 1;
                    pc.tsize = total_size;
                    pc.tcsize = total_csize;
                }
                maybe_draw();
                let now_us = Timer::micros();
                if now_us.saturating_sub(last_progress_us) >= 100_000 {
                    let pkt = postcard::to_slice(
                        &UdpRequest::ActionProgress(uploaded_chunks.len(), total),
                        &mut progress_buf,
                    )?;
                    udp.send_to(server_address, pkt).await?;
                    last_progress_us = now_us;
                }
            }
        }
        pipeline.finish().await?;
        let pkt = postcard::to_slice(
            &UdpRequest::ActionProgress(uploaded_chunks.len(), total),
            &mut progress_buf,
        )?;
        udp.send_to(server_address, pkt).await?;
        Ok::<_, Error>(uploaded_chunks)
    };

    let ((), (), chunk_hashes) = futures::try_join!(reader_task, hash_task, upload_task)?;
    ui::update_content(draw);

    rpc.upload_image(&Image {
        boot_option_id: boid,
        boot_entry: bo_command.to_vec(),
        disk: chunk_hashes,
    })
    .await?;

    log::info!(
        "image saved. Total size {}, total csize {}",
        BytesFmt(total_size as u64),
        BytesFmt(total_csize as u64)
    );

    Ok(())
}
