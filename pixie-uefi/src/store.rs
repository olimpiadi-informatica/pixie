use alloc::vec::Vec;
use core::cell::RefCell;
use core::fmt::Write;
use core::net::SocketAddrV4;

use log::info;
use lz4_flex::compress;
use pixie_shared::rpc::HAS_CHUNKS_BATCH_SIZE;
use pixie_shared::util::BytesFmt;
use pixie_shared::{Chunk, Image, MAX_CHUNK_SIZE, Offset, UdpRequest};

use crate::os::boot_options::BootOptions;
use crate::os::error::{Error, Result};
use crate::os::net::UdpSocket;
use crate::os::ui::DrawArea;
use crate::os::{disk, memory, ui};
use crate::rpc::RpcClient;
use crate::{MIN_MEMORY, parse_disk};

#[derive(Debug)]
pub struct ChunkInfo {
    pub start: Offset,
    pub size: usize,
}

struct PushingChunks {
    total: usize,
    hashed: usize,
    checked: usize,
    pushed: usize,
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
                hashed,
                checked,
                pushed,
                tsize,
                tcsize,
            }) => {
                writeln!(
                    draw_area,
                    "Pushed {pushed} / {checked} / {hashed} / {total} chunks"
                )
                .unwrap();
                writeln!(draw_area, "total size {tsize}, compressed {tcsize}").unwrap();
            }
        }
    };

    ui::update_content(draw);

    let boid = BootOptions::reboot_target().expect("Could not find reboot target");
    let bo_command = BootOptions::get(boid);

    let mut disk = disk::Disk::largest();
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
        (free_mem.saturating_sub(MIN_MEMORY) as usize / (4 * MAX_CHUNK_SIZE)).max(32);
    log::debug!(
        "Free memory: {}. Channel size: {channel_size}",
        BytesFmt(free_mem)
    );

    stats.replace(State::PushingChunks(PushingChunks {
        total,
        hashed: 0,
        checked: 0,
        pushed: 0,
        tsize: 0,
        tcsize: 0,
    }));
    ui::update_content(draw);

    let (tx, rx) = thingbuf::mpsc::channel(channel_size);

    let task1 = async {
        let tx = tx;
        for chunk_info in chunks {
            let mut data = vec![0; chunk_info.size];
            disk.read(chunk_info.start as u64, &mut data).await?;
            let cdata = compress(&data);
            let hash = blake3::hash(&data).into();
            let chunk = Chunk {
                hash,
                start: chunk_info.start,
                size: chunk_info.size,
                csize: cdata.len(),
            };
            stats.borrow_mut().as_pushing_chunks_mut().hashed += 1;
            ui::update_content(draw);
            tx.send((chunk, cdata)).await.expect("receiver dropped");
        }
        Ok::<_, Error>(())
    };

    let task2 = async {
        let mut uploaded_chunks = Vec::new();
        while let Some(first) = rx.recv().await {
            let mut batch = Vec::with_capacity(HAS_CHUNKS_BATCH_SIZE);
            batch.push(first);
            while batch.len() < HAS_CHUNKS_BATCH_SIZE {
                let Ok(item) = rx.try_recv() else {
                    break;
                };
                batch.push(item);
            }

            let hashes = batch.iter().map(|(c, _)| c.hash).collect();
            let present = rpc.has_chunks(hashes).await?;
            {
                let mut s = stats.borrow_mut();
                s.as_pushing_chunks_mut().checked += batch.len();
            }
            ui::update_content(draw);

            for ((chunk, cdata), has_chunk) in batch.into_iter().zip(present) {
                if !has_chunk {
                    rpc.upload_chunk(&cdata).await?;
                }
                uploaded_chunks.push(chunk);
                total_size += chunk.size;
                total_csize += chunk.csize;
                {
                    let mut s = stats.borrow_mut();
                    let pc = s.as_pushing_chunks_mut();
                    pc.pushed += 1;
                    pc.tsize = total_size;
                    pc.tcsize = total_csize;
                }
                ui::update_content(draw);
                udp.send_to(
                    server_address,
                    &postcard::to_allocvec(&UdpRequest::ActionProgress(
                        uploaded_chunks.len(),
                        total,
                    ))?,
                )
                .await?;
            }
        }
        Ok::<_, Error>(uploaded_chunks)
    };

    let ((), chunk_hashes) = futures::try_join!(task1, task2)?;

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
