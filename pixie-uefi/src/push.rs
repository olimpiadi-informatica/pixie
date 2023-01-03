use alloc::{string::String, sync::Arc, vec::Vec};
use core::cell::RefCell;

use lz4_flex::compress;
use uefi::proto::console::text::Color;

use crate::os::{
    disk::Disk,
    error::{Error, Result},
    mpsc, MessageKind, TcpStream, UefiOS,
};
use pixie_shared::{Address, Chunk, Image, Offset, TcpRequest, UdpRequest, CHUNK_SIZE};

#[derive(Debug)]
struct ChunkInfo {
    start: Offset,
    size: usize,
}

// Returns chunks *relative to the start of the partition*.
async fn get_ext4_chunks(disk: &Disk, start: u64, end: u64) -> Result<Option<Vec<ChunkInfo>>> {
    fn le16(buf: &[u8], lo: usize) -> u16 {
        (0..2).map(|i| (buf[lo + i] as u16) << (8 * i)).sum()
    }

    fn le32(buf: &[u8], lo: usize) -> u32 {
        (0..4).map(|i| (buf[lo + i] as u32) << (8 * i)).sum()
    }

    fn le64_32_32(buf: &[u8], lo: usize, hi: usize) -> u64 {
        (0..4)
            .map(|i| ((buf[lo + i] as u64) << (8 * i)) + ((buf[hi + i] as u64) << (8 * i + 32)))
            .sum()
    }

    fn has_superblock(group: usize) -> bool {
        if group <= 1 {
            return true;
        }

        for d in [3, 5, 7] {
            let mut p = 1;
            while p < group {
                p *= d;
            }
            if p == group {
                return true;
            }
        }

        false
    }

    if start + 2048 > end {
        // Not an ext4 partition.
        return Ok(None);
    }
    // Read superblock.
    let mut superblock = [0; 1024];
    disk.read(start + 1024, &mut superblock).await?;

    let magic = le16(&superblock, 0x38);
    if magic != 0xEF53 {
        return Ok(None);
    }

    let feature_incompat = le32(&superblock, 0x60);
    if feature_incompat & 0x80 == 0 {
        // INCOMPAT_64BIT flag
        return Ok(None);
    }

    let feature_ro_compat = le32(&superblock, 0x64);
    if feature_ro_compat & 0x1 == 0 {
        // RO_COMPAT_SPARSE_SUPER flag
        return Ok(None);
    }

    let blocks_count = le64_32_32(&superblock, 0x4, 0x150);
    let log_block_size = le32(&superblock, 0x18);
    assert!(blocks_count.checked_shl(10 + log_block_size).is_some());
    let block_size = 1u64 << (10 + log_block_size);

    let blocks_per_group = le32(&superblock, 0x20) as u64;
    let groups = (blocks_count + blocks_per_group - 1) / blocks_per_group;

    let first_data_block = le32(&superblock, 0x14) as u64;
    let desc_size = le16(&superblock, 0xfe) as u64;
    let reserved_gdt_blocks = le16(&superblock, 0xce);

    let blocks_for_special_group = 1
        + ((desc_size * groups + block_size - 1) / block_size) as usize
        + reserved_gdt_blocks as usize;

    let mut group_descriptors = vec![0; (desc_size * groups) as usize];
    let mut bitmap = vec![0; block_size as usize];
    disk.read(
        start + block_size * (first_data_block + 1),
        &mut group_descriptors,
    )
    .await?;

    let mut ans = Vec::new();

    for (group, group_descriptor) in group_descriptors.chunks(desc_size as usize).enumerate() {
        let flags = le16(group_descriptor, 0x12);
        if flags & 0x2 != 0 {
            // EXT4_BG_BLOCK_UNINIT
            if has_superblock(group) {
                for block in 0..blocks_for_special_group {
                    if group * blocks_per_group as usize + block < blocks_count as usize {
                        ans.push(ChunkInfo {
                            start: block_size as usize
                                * (group * blocks_per_group as usize + block),
                            size: block_size as usize,
                        });
                    }
                }
            }
        } else {
            let block_bitmap = le64_32_32(group_descriptor, 0x0, 0x20);

            disk.read(start + block_size * block_bitmap, &mut bitmap)
                .await?;

            for block in 0..8 * block_size as usize {
                let is_used = bitmap[block / 8] >> (block % 8) & 1 != 0;
                if is_used && group * blocks_per_group as usize + block < blocks_count as usize {
                    ans.push(ChunkInfo {
                        start: block_size as usize * (group * blocks_per_group as usize + block),
                        size: block_size as usize,
                    });
                }
            }
        }
    }

    Ok(Some(ans))
}

async fn save_image(stream: &TcpStream, name: String, image: Image) -> Result<()> {
    let req = TcpRequest::UploadImage(name, image);
    let buf = postcard::to_allocvec(&req)?;
    stream.send_u64_le(buf.len() as u64).await?;
    stream.send(&buf).await?;
    let len = stream.recv_u64_le().await?;
    assert_eq!(len, 0);
    Ok(())
}

enum State {
    ReadingPartitions,
    PushingChunks {
        cur: usize,
        total: usize,
        tsize: usize,
        tcsize: usize,
    },
}

pub async fn push(os: UefiOS, server_address: Address, image: String) -> Result<()> {
    let stats = Arc::new(RefCell::new(State::ReadingPartitions));
    let stats2 = stats.clone();
    os.set_ui_drawer(move |os| match &*stats2.borrow() {
        State::ReadingPartitions => {
            os.write_with_color("Reading partitions...", Color::White, Color::Black)
        }
        State::PushingChunks {
            cur,
            total,
            tsize,
            tcsize,
        } => {
            os.write_with_color(
                &format!("Pushed {} out of {} chunks\n", cur, total),
                Color::White,
                Color::Black,
            );
            os.write_with_color(
                &format!("total size {}, compressed {}\n", tsize, tcsize),
                Color::White,
                Color::Black,
            );
        }
    });

    let mut disk = os.open_first_disk();
    let disk_size = disk.size() as usize;
    let partitions = disk.partitions().expect("disk is not GPT");

    let mut pos = 0usize;
    let mut chunks = vec![];
    for partition in partitions {
        let begin = partition.byte_start as usize;
        let end = partition.byte_end as usize;

        if pos < begin {
            chunks.push(ChunkInfo {
                start: pos,
                size: (begin - pos),
            });
        }

        if let Some(e4chunks) = get_ext4_chunks(&disk, begin as u64, end as u64).await? {
            for ChunkInfo { start, size } in e4chunks {
                chunks.push(ChunkInfo {
                    start: start + begin,
                    size,
                });
            }
        } else {
            chunks.push(ChunkInfo {
                start: begin,
                size: (end - begin),
            });
        }

        pos = end;
    }

    if pos < disk_size {
        chunks.push(ChunkInfo {
            start: pos,
            size: disk_size - pos,
        });
    }

    // Split up chunks.
    let mut final_chunks = Vec::<ChunkInfo>::new();
    for ChunkInfo { mut start, size } in chunks {
        let end = start + size;

        if let Some(last) = final_chunks.last() {
            assert!(last.start + last.size <= start);
            if last.start + last.size == start {
                start = last.start;
                final_chunks.pop();
            }
        }

        while start < end {
            let split = end.min((start / CHUNK_SIZE + 1) * CHUNK_SIZE);
            final_chunks.push(ChunkInfo {
                start,
                size: split - start,
            });
            start = split;
        }
    }

    let udp = os.udp_bind(None).await?;
    let stream_get_csize = os.connect(server_address.ip, server_address.port).await?;
    let stream_upload_chunk = os.connect(server_address.ip, server_address.port).await?;

    let total = final_chunks.len();

    let mut total_size = 0;
    let mut total_csize = 0;

    let (tx1, mut rx1) = mpsc::channel(32);
    let (tx2, mut rx2) = mpsc::channel(32);
    let (tx3, mut rx3) = mpsc::channel(32);
    let (tx4, mut rx4) = mpsc::channel(32);
    let (tx5, mut rx5) = mpsc::channel(32);

    let task1 = async {
        let mut tx1 = tx1;
        for chnk in final_chunks {
            let mut data = vec![0; chnk.size];
            disk.read(chnk.start as u64, &mut data).await?;
            let hash = blake3::hash(&data).into();
            tx1.send((chnk.start, hash, data)).await;
        }
        Ok::<_, Error>(())
    };

    let task2 = async {
        let mut tx2 = tx2;
        while let Some((start, hash, data)) = rx1.recv().await {
            let req = TcpRequest::GetChunkSize(hash);
            let buf = postcard::to_allocvec(&req)?;
            stream_get_csize.send_u64_le(buf.len() as u64).await?;
            stream_get_csize.send(&buf).await?;
            tx2.send((start, hash, data)).await;
        }
        Ok(())
    };

    let task3 = async {
        let mut tx3 = tx3;
        while let Some((start, hash, data)) = rx2.recv().await {
            let len = stream_get_csize.recv_u64_le().await?;
            let mut buf = vec![0; len as usize];
            stream_get_csize.recv(&mut buf).await?;
            let csize: Option<usize> = postcard::from_bytes(&buf)?;
            tx3.send((start, hash, data, csize)).await;
        }
        Ok(())
    };

    let task4 = async {
        let mut tx4 = tx4;
        while let Some((start, hash, data, csize)) = rx3.recv().await {
            let (csize, cdata) = match csize {
                Some(csize) => (csize, None),
                None => {
                    let cdata = compress(&data);
                    (cdata.len(), Some(cdata))
                }
            };
            tx4.send((start, hash, data, csize, cdata)).await;
        }
        Ok(())
    };

    let task5 = async {
        let mut tx5 = tx5;
        while let Some((start, hash, data, csize, cdata)) = rx4.recv().await {
            let check_ack = cdata.is_some();
            if let Some(cdata) = cdata {
                let req = TcpRequest::UploadChunk(hash, cdata);
                let buf = postcard::to_allocvec(&req)?;
                stream_upload_chunk.send_u64_le(buf.len() as u64).await?;
                stream_upload_chunk.send(&buf).await?;
            }
            tx5.send((start, hash, data, csize, check_ack)).await;
        }
        Ok(())
    };

    let task6 = async {
        stats.replace(State::PushingChunks {
            cur: 0,
            total,
            tsize: 0,
            tcsize: 0,
        });

        let mut chunks = Vec::new();
        while let Some((start, hash, data, csize, check_ack)) = rx5.recv().await {
            if check_ack {
                let len = stream_upload_chunk.recv_u64_le().await?;
                assert_eq!(len, 0);
            }
            chunks.push(Chunk {
                hash,
                start,
                size: data.len(),
                csize,
            });

            total_size += data.len();
            total_csize += csize;

            stats.replace(State::PushingChunks {
                cur: chunks.len(),
                total,
                tsize: total_size,
                tcsize: total_csize,
            });
            udp.send(
                server_address.ip,
                server_address.port,
                &postcard::to_allocvec(&UdpRequest::ActionProgress(chunks.len(), total))?,
            )
            .await?;
        }
        Ok(chunks)
    };

    let ((), (), (), (), (), chunk_hashes) =
        futures::try_join!(task1, task2, task3, task4, task5, task6)?;

    let bo = os.boot_options();
    let boid = bo.order()[1];
    let bo_command = bo.get(boid);

    save_image(
        &stream_upload_chunk,
        image.clone(),
        Image {
            boot_option_id: boid,
            boot_entry: bo_command,
            disk: chunk_hashes,
        },
    )
    .await?;

    stream_get_csize.close_send().await;
    stream_get_csize.force_close().await;

    stream_upload_chunk.close_send().await;
    // TODO(virv): this could be better
    stream_upload_chunk.force_close().await;

    os.append_message(
        format!(
            "image saved at {:?}/{}. Total size {total_size}, total csize {total_csize}",
            server_address, image,
        ),
        MessageKind::Info,
    );

    Ok(())
}
