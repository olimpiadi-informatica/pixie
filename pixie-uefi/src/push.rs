use alloc::{string::String, sync::Arc, vec::Vec};
use core::cell::RefCell;

use blake3::Hash;
use miniz_oxide::deflate::compress_to_vec;
use uefi::proto::console::text::Color;

use crate::os::{disk::Disk, error::Result, MessageKind, TcpStream, UefiOS};
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

async fn get_chunk_csize(stream: &TcpStream, hash: &Hash) -> Result<Option<usize>> {
    let req = TcpRequest::GetChunkSize((*hash).into());
    let mut buf = serde_json::to_vec(&req)?;
    stream.send_u64_le(buf.len() as u64).await?;
    stream.send(&buf).await?;
    let len = stream.recv_u64_le().await?;
    buf.resize(len as usize, 0);
    stream.recv(&mut buf).await?;
    Ok(serde_json::from_slice(&buf)?)
}

async fn save_chunk(stream: &TcpStream, hash: &Hash, data: &[u8]) -> Result<()> {
    let req = TcpRequest::UploadChunk((*hash).into(), data.into());
    let buf = serde_json::to_vec(&req)?;
    stream.send_u64_le(buf.len() as u64).await?;
    stream.send(&buf).await?;
    let len = stream.recv_u64_le().await?;
    assert_eq!(len, 0);
    Ok(())
}

async fn save_image(stream: &TcpStream, name: String, image: Image) -> Result<()> {
    let req = TcpRequest::UploadImage(name, image);
    let buf = serde_json::to_vec(&req)?;
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

pub async fn push(
    os: UefiOS,
    server_address: Address,
    image: String,
) -> Result<()> {
    let stats = Arc::new(RefCell::new(State::ReadingPartitions));

    let udp = os.udp_bind(None).await?;
    let stream = os.connect(server_address.ip, server_address.port).await?;

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

    let total = final_chunks.len();

    let mut total_size = 0;
    let mut total_csize = 0;

    let mut chunk_hashes = vec![];
    for (idx, chnk) in final_chunks.into_iter().enumerate() {
        stats.replace(State::PushingChunks {
            cur: idx,
            total,
            tsize: total_size,
            tcsize: total_csize,
        });

        udp.send(
            server_address.ip,
            server_address.port,
            &serde_json::to_vec(&UdpRequest::ActionProgress(idx, total))?,
        )
        .await?;

        let mut data = vec![0; chnk.size];
        disk.read(chnk.start as u64, &mut data).await?;
        let hash = blake3::hash(&data);
        let csize = match get_chunk_csize(&stream, &hash).await? {
            Some(csize) => csize,
            None => {
                // TODO(veluca): consider passing in the compression level.
                let cdata = compress_to_vec(&data, 2);
                save_chunk(&stream, &hash, &cdata).await?;
                cdata.len()
            }
        };

        total_size += chnk.size;
        total_csize += csize;
        chunk_hashes.push(Chunk {
            hash: hash.into(),
            start: chnk.start,
            size: chnk.size,
            csize,
        })
    }

    let bo = os.boot_options();
    let boid = bo.order()[1];
    let bo_command = bo.get(boid);

    save_image(
        &stream,
        image.clone(),
        Image {
            boot_option_id: boid,
            boot_entry: bo_command,
            disk: chunk_hashes,
        },
    )
    .await?;

    stream.close_send().await;
    // TODO(virv): this could be better
    stream.force_close().await;

    os.append_message(
        format!(
            "image saved at {:?}/{}. Total size {total_size}, total csize {total_csize}",
            server_address, image,
        ),
        MessageKind::Info,
    );

    Ok(())
}
