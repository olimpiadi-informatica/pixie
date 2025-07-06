use crate::{
    os::{
        disk::Disk,
        error::{Error, Result},
    },
    store::ChunkInfo,
};
use alloc::vec::Vec;

fn le16(buf: &[u8], lo: usize) -> u16 {
    (0..2).map(|i| (buf[lo + i] as u16) << (8 * i)).sum()
}

fn le32(buf: &[u8], lo: usize) -> u32 {
    (0..4).map(|i| (buf[lo + i] as u32) << (8 * i)).sum()
}

fn le64(buf: &[u8], lo: usize) -> u64 {
    (0..8).map(|i| (buf[lo + i] as u64) << (8 * i)).sum()
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
    let groups = blocks_count.div_ceil(blocks_per_group);

    let first_data_block = le32(&superblock, 0x14) as u64;
    let desc_size = le16(&superblock, 0xfe) as u64;
    let reserved_gdt_blocks = le16(&superblock, 0xce);

    let blocks_for_special_group =
        1 + (desc_size * groups).div_ceil(block_size) as usize + reserved_gdt_blocks as usize;

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
    if end - start < 512 {
        return Ok(None);
    }

    let mut boot_sector = [0u8; 512];
    disk.read(start, &mut boot_sector).await?;

    if &boot_sector[3..11] != b"NTFS    " {
        return Ok(None);
    }

    let bytes_per_sector = le16(&boot_sector, 0x0b) as usize;

    let sectors_per_cluster = match boot_sector[0x0d] {
        x @ 0..=127 => x as usize,
        x @ 225..=255 => 1 << -(x as i8),
        x @ 128..=224 => panic!("too many sectors per cluster: {}", x),
    };
    let bytes_per_cluster = bytes_per_sector * sectors_per_cluster;
    let num_clusters = (end as usize - start as usize).div_ceil(bytes_per_cluster);

    let bytes_per_file_record = match boot_sector[0x40] {
        x @ 0..=127 => x as usize * bytes_per_cluster,
        x @ 225..=255 => 1 << -(x as i8),
        x @ 128..=224 => panic!("too many bytes per file record: {}", x),
    };

    let mft_cluster_number = le64(&boot_sector, 0x30) as usize;
    let mft_address = bytes_per_cluster * mft_cluster_number;

    let bitmap_entry_address = mft_address + 6 * bytes_per_file_record;
    let mut bitmap_entry = [0u8; 1024];
    disk.read(start + bitmap_entry_address as u64, &mut bitmap_entry)
        .await
        .map_err(|e| Error::Generic(format!("failed to read bitmap entry: {e}")))?;

    let mut attribute_offset = le16(&bitmap_entry, 0x14) as usize;
    while le32(&bitmap_entry, attribute_offset) != 0x80 {
        attribute_offset += le32(&bitmap_entry, attribute_offset + 4) as usize;
    }

    let non_resident_flag = bitmap_entry[attribute_offset + 8];
    assert_eq!(non_resident_flag, 1);

    let mut start_vcn = le64(&bitmap_entry, attribute_offset + 0x10) as usize;
    let last_vcn = le64(&bitmap_entry, attribute_offset + 0x18) as usize;
    let mut data_run_offset =
        attribute_offset + le16(&bitmap_entry, attribute_offset + 0x20) as usize;

    let mut cnt = 0;
    let mut chunks = Vec::new();

    while start_vcn <= last_vcn {
        let ctrl_byte = bitmap_entry[data_run_offset];

        let length_len = (ctrl_byte & 0x0f) as usize;
        let length =
            (le64(&bitmap_entry, data_run_offset + 1) & ((1 << (8 * length_len)) - 1)) as usize;

        let offset_len = (ctrl_byte >> 4) as usize;
        let offset = (le64(&bitmap_entry, data_run_offset + 1 + length_len)
            & ((1 << (8 * offset_len)) - 1)) as usize;

        let mut buf = vec![0u8; bytes_per_cluster];
        for i in 0..length {
            let x = start + (offset + i) as u64 * bytes_per_cluster as u64;
            disk.read(x, &mut buf).await.map_err(|e| {
                Error::Generic(format!("failed to read bitmap content at {x}: {e}"))
            })?;

            for &byte in &buf {
                for bit in 0..8 {
                    if cnt < num_clusters as u64 {
                        if byte >> bit & 1 != 0 {
                            chunks.push(ChunkInfo {
                                start: cnt as usize * bytes_per_cluster,
                                size: bytes_per_cluster,
                            });
                        }
                        cnt += 1;
                    }
                }
            }
        }

        start_vcn += length;
        data_run_offset += 1 + length_len + offset_len;
    }

    Ok(Some(chunks))
}

async fn get_swap_chunks(disk: &Disk, start: u64, end: u64) -> Result<Option<Vec<ChunkInfo>>> {
    if end - start < 4096 {
        return Ok(None);
    }

    let mut first_chunk = [0u8; 4096];
    disk.read(start, &mut first_chunk).await?;

    if &first_chunk[4096 - 10..] != b"SWAPSPACE2" {
        return Ok(None);
    }

    Ok(Some(vec![ChunkInfo {
        start: 0,
        size: 4096,
    }]))
}

/// Returns chunks *relative to the start of the partition*.
async fn parse_partition(disk: &Disk, start: u64, end: u64) -> Result<Vec<ChunkInfo>> {
    if let Some(chunks) = get_ext4_chunks(disk, start, end).await? {
        Ok(chunks)
    } else if let Some(chunks) = get_ntfs_chunks(disk, start, end).await? {
        Ok(chunks)
    } else if let Some(chunks) = get_swap_chunks(disk, start, end).await? {
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
    let Ok(partitions) = disk.partitions() else {
        return Ok(None);
    };

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

        let part_chunks = parse_partition(disk, begin as u64, end as u64).await?;
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
