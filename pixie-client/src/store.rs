use alloc::vec::Vec;
use core::cell::RefCell;
use core::fmt::Write;
use core::net::SocketAddrV4;

use log::info;
use lz4_flex::compress;
use pixie_shared::util::BytesFmt;
use pixie_shared::{Chunk, ChunkHash, Image, MAX_CHUNK_SIZE, Offset, RpcRequest, UdpRequest};

use crate::os::boot_options::BootOptions;
use crate::os::error::{Error, Result};
use crate::os::net::UdpSocket;
use crate::os::ui::DrawArea;
use crate::os::{disk, memory, ui};
use crate::{MIN_MEMORY, parse_disk};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkInfo {
    pub start: Offset,
    pub size: usize,
}

async fn save_image(udp: &UdpSocket, server_address: SocketAddrV4, image: Image) -> Result<()> {
    let buf = postcard::to_allocvec(&image)?;
    udp.upload_blob(server_address, &buf).await?;
    udp.rpc::<()>(
        server_address,
        RpcRequest::CommitImage {
            size: buf.len() as u32,
        },
    )
    .await
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
            stats.borrow_mut().as_pushing_chunks_mut().read_chunks += 1;
            ui::update_content(draw);
            tx.send((chunk, cdata)).await.expect("receiver dropped");
        }
        Ok::<_, Error>(())
    };

    let task2 = async {
        let mut chunks = Vec::new();
        let mut batch = Vec::with_capacity(32);
        let mut uploaded_in_batch: Vec<ChunkHash> = Vec::with_capacity(32);
        let mut progress_buf = [0u8; 32];
        while let Some(first) = rx.recv().await {
            batch.clear();
            batch.push(first);
            while batch.len() < 32 {
                match rx.try_recv() {
                    Ok(item) => batch.push(item),
                    Err(_) => break,
                }
            }

            let hashes: Vec<_> = batch.iter().map(|(c, _)| c.hash).collect();
            let present_mask: u32 = udp
                .rpc(server_address, RpcRequest::HasChunks(hashes))
                .await?;

            uploaded_in_batch.clear();
            for (i, (chunk, cdata)) in batch.drain(..).enumerate() {
                if (present_mask & (1u32 << i)) == 0 && !uploaded_in_batch.contains(&chunk.hash) {
                    udp.upload_blob(server_address, &cdata).await?;
                    udp.rpc::<()>(
                        server_address,
                        RpcRequest::CommitChunk {
                            hash: chunk.hash,
                            size: cdata.len() as u32,
                        },
                    )
                    .await?;
                    uploaded_in_batch.push(chunk.hash);
                }
                chunks.push(chunk);

                total_size += chunk.size;
                total_csize += chunk.csize;

                {
                    let mut st = stats.borrow_mut();
                    let pc = st.as_pushing_chunks_mut();
                    pc.pushed_chunks += 1;
                    pc.tsize = total_size;
                    pc.tcsize = total_csize;
                }
                ui::update_content(draw);
                let pkt = postcard::to_slice(
                    &UdpRequest::ActionProgress(chunks.len(), total),
                    &mut progress_buf,
                )?;
                udp.send_to(server_address, pkt).await?;
            }
        }
        Ok(chunks)
    };

    let ((), chunk_hashes) = futures::try_join!(task1, task2)?;

    save_image(
        &udp,
        server_address,
        Image {
            boot_option_id: boid,
            boot_entry: bo_command.to_vec(),
            disk: chunk_hashes,
        },
    )
    .await?;

    log::info!(
        "image saved. Total size {}, total csize {}",
        BytesFmt(total_size as u64),
        BytesFmt(total_csize as u64)
    );

    Ok(())
}
