use alloc::vec::Vec;

use crate::{
    os::{disk::Disk, error::Result},
    push::ChunkInfo,
};

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

async fn get_ext4_chunks(disk: &Disk, start: u64, end: u64) -> Result<Option<Vec<ChunkInfo>>> {
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

async fn get_ntfs_chunks(disk: &Disk, start: u64, end: u64) -> Result<Option<Vec<ChunkInfo>>> {
    Ok(None)
}

/// Returns chunks *relative to the start of the partition*.
async fn parse_partition(disk: &Disk, start: u64, end: u64) -> Result<Vec<ChunkInfo>> {
    if let Some(chunks) = get_ext4_chunks(disk, start, end).await? {
        Ok(chunks)
    } else if let Some(chunks) = get_ntfs_chunks(disk, start, end).await? {
        Ok(chunks)
    } else {
        Ok(vec![ChunkInfo {
            start: 0,
            size: (end - start) as usize,
        }])
    }
}

async fn parse_gpt(disk: &mut Disk) -> Result<Option<Vec<ChunkInfo>>> {
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

        let part_chunks = parse_partition(&disk, begin as u64, end as u64).await?;
        for ChunkInfo { start, size } in part_chunks {
            chunks.push(ChunkInfo {
                start: start + begin,
                size,
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

    Ok(Some(chunks))
}

pub async fn parse_disk(disk: &mut Disk) -> Result<Vec<ChunkInfo>> {
    if let Some(chunks) = parse_gpt(disk).await? {
        Ok(chunks)
    } else {
        parse_partition(disk, 0, disk.size()).await
    }
}
