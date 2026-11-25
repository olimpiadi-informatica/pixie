use alloc::vec::Vec;

use super::{le16, le32, le64_32_32};
use crate::os::disk::DiskIo;
use crate::os::error::Result;
use crate::store::ChunkInfo;

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

pub async fn get_ext4_chunks<D: DiskIo + ?Sized>(
    disk: &mut D,
    start: u64,
    end: u64,
) -> Result<Option<Vec<ChunkInfo>>> {
    if start + 2048 > end {
        return Ok(None);
    }
    let mut superblock = [0; 1024];
    disk.read(start + 1024, &mut superblock).await?;

    let magic = le16(&superblock, 0x38);
    if magic != 0xEF53 {
        return Ok(None);
    }

    let feature_compat = le32(&superblock, 0x5c);

    // Require INCOMPAT_64BIT (0x80); fall back to raw copy if INCOMPAT_META_BG (0x10) is set.
    let feature_incompat = le32(&superblock, 0x60);
    if feature_incompat & 0x80 == 0 || feature_incompat & 0x10 != 0 {
        return Ok(None);
    }

    // Fall back to raw copy if RO_COMPAT_BIGALLOC (0x200) is set (cluster bitmaps instead of block bitmaps).
    let feature_ro_compat = le32(&superblock, 0x64);
    if feature_ro_compat & 0x1 == 0 && feature_compat & 0x200 == 0 {
        return Ok(None);
    }
    if feature_ro_compat & 0x200 != 0 {
        return Ok(None);
    }

    let sparse_super2 = feature_compat & 0x200 != 0;
    let backup_bg0 = le32(&superblock, 0x24c) as usize;
    let backup_bg1 = le32(&superblock, 0x250) as usize;
    let group_has_superblock = |group: usize| -> bool {
        if sparse_super2 {
            group == 0
                || (backup_bg0 != 0 && group == backup_bg0)
                || (backup_bg1 != 0 && group == backup_bg1)
        } else {
            has_superblock(group)
        }
    };

    let blocks_count = le64_32_32(&superblock, 0x4, 0x150);
    let log_block_size = le32(&superblock, 0x18);
    if log_block_size > 16 || blocks_count.checked_shl(10 + log_block_size).is_none() {
        return Ok(None);
    }
    let block_size = 1u64 << (10 + log_block_size);

    let blocks_per_group = le32(&superblock, 0x20) as u64;
    if blocks_per_group == 0 {
        return Ok(None);
    }
    let groups = blocks_count.div_ceil(blocks_per_group);

    let first_data_block = le32(&superblock, 0x14) as u64;
    let desc_size = le16(&superblock, 0xfe) as u64;
    if desc_size < 64 {
        return Ok(None);
    }
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
            if group_has_superblock(group) {
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
