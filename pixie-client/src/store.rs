use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::fmt::Write;
use core::net::SocketAddrV4;

use log::info;
use lz4_flex::compress;
use pixie_shared::rpc::HAS_CHUNKS_BATCH_SIZE;
use pixie_shared::util::BytesFmt;
use pixie_shared::{Chunk, ChunkHash, Image, MAX_CHUNK_SIZE, Offset, UdpRequest};

use crate::os::boot_options::BootOptions;
use crate::os::error::{Error, Result};
use crate::os::net::UdpSocket;
use crate::os::timer::Timer;
use crate::os::ui::DrawArea;
use crate::os::{disk, memory, ui};
use crate::rpc::RpcClient;
use crate::{MIN_MEMORY, parse_disk};

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

    let total = chunks.len();

    let mut total_size = 0;
    let mut total_csize = 0;

    let free_mem = memory::stats().free;
    let channel_size =
        (free_mem.saturating_sub(MIN_MEMORY) as usize / (4 * MAX_CHUNK_SIZE)).clamp(32, 128);
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

    let (tx, rx) = thingbuf::mpsc::channel::<(Chunk, Option<Vec<u8>>)>(channel_size);
    let last_draw_us = Cell::new(Timer::micros());
    let maybe_draw = || {
        let now = Timer::micros();
        if now.saturating_sub(last_draw_us.get()) >= 100_000 {
            ui::update_content(draw);
            last_draw_us.set(now);
        }
    };

    let task1 = async {
        let tx = tx;
        let mut read_buf = vec![0u8; MAX_CHUNK_SIZE];
        let mut seen_hashes: BTreeMap<ChunkHash, usize> = BTreeMap::new();
        for chunk_info in chunks {
            if chunk_info.size > read_buf.len() {
                read_buf.resize(chunk_info.size, 0);
            }
            let data = &mut read_buf[..chunk_info.size];
            disk.read(chunk_info.start as u64, data).await?;
            let hash: ChunkHash = blake3::hash(data).into();
            let (csize, cdata) = if let Some(&csize) = seen_hashes.get(&hash) {
                (csize, None)
            } else {
                let cdata = compress(data);
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
        Ok::<_, Error>(())
    };

    let task2 = async {
        let mut uploaded_chunks = Vec::new();
        let mut batch = Vec::with_capacity(HAS_CHUNKS_BATCH_SIZE);
        let mut progress_buf = [0u8; 32];
        let mut last_progress_us = Timer::micros();
        while let Some(first) = rx.recv().await {
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
                rpc.has_chunks(hashes).await?
            };

            let mut present_iter = present.into_iter();
            for (chunk, cdata) in batch.drain(..) {
                if let Some(cdata) = cdata {
                    let has_chunk = present_iter.next().unwrap_or(false);
                    if !has_chunk {
                        rpc.upload_chunk(&cdata).await?;
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
        let pkt = postcard::to_slice(
            &UdpRequest::ActionProgress(uploaded_chunks.len(), total),
            &mut progress_buf,
        )?;
        udp.send_to(server_address, pkt).await?;
        Ok::<_, Error>(uploaded_chunks)
    };

    let ((), chunk_hashes) = futures::try_join!(task1, task2)?;
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
