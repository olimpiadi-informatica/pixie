use alloc::vec::Vec;

use super::{le16, le32};
use crate::os::disk::Disk;
use crate::os::error::Result;
use crate::store::ChunkInfo;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Type {
    Fat12,
    Fat16,
    Fat32,
}

impl Type {
    fn bits(self) -> usize {
        match self {
            Type::Fat12 => 12,
            Type::Fat16 => 16,
            Type::Fat32 => 32,
        }
    }

    fn index(self, buf: &[u8], idx: usize) -> u64 {
        match self {
            Type::Fat12 => {
                let byte = idx * 3 / 2;
                ((le16(buf, byte) >> ((idx & 1) * 4)) & 0x0FFF) as u64
            }
            Type::Fat16 => le16(buf, idx * 2) as u64,
            Type::Fat32 => le32(buf, idx * 4) as u64 & 0x0FFFFFFF,
        }
    }
}

pub async fn get_fat_chunks(disk: &Disk, start: u64, end: u64) -> Result<Option<Vec<ChunkInfo>>> {
    if end - start < 512 {
        return Ok(None);
    }

    let mut ebpb = [0; 512];
    disk.read(start, &mut ebpb).await?;
    let fat_type = if &ebpb[0x52..0x5A] == b"FAT32   " {
        Type::Fat32
    } else if &ebpb[0x36..0x3E] == b"FAT16   " {
        Type::Fat16
    } else if &ebpb[0x36..0x3E] == b"FAT12   " {
        Type::Fat12
    } else {
        return Ok(None);
    };

    let sector_size = le16(&ebpb, 0x0B) as u64;
    let sectors_per_cluster = ebpb[0x0D] as u64;
    let cluster_size = sector_size * sectors_per_cluster;
    let total_sectors_short = le16(&ebpb, 0x13) as u64;
    let total_sectors_long = le32(&ebpb, 0x20) as u64;
    let total_sectors = if total_sectors_short > 0 {
        total_sectors_short
    } else {
        total_sectors_long
    };
    log::trace!(
        "FAT partition: sector_size={sector_size}, sectors_per_cluster={sectors_per_cluster}, cluster_size={cluster_size}, total_sectors={total_sectors}"
    );

    let first_fat_sector = le16(&ebpb, 0x0E) as u64;
    let fat_size = match fat_type {
        Type::Fat12 | Type::Fat16 => le16(&ebpb, 0x16) as u64,
        Type::Fat32 => le32(&ebpb, 0x24) as u64,
    };
    let table_count = ebpb[0x10] as u64;
    log::trace!(
        "FAT partition: fat_bits={}, first_fat_sector={first_fat_sector}, fat_size={fat_size}, table_count={table_count}",
        fat_type.bits(),
    );

    let root_dir_entries = le16(&ebpb, 0x11) as u64;
    let root_dir_sectors = (root_dir_entries * 32).div_ceil(sector_size);
    log::trace!(
        "FAT partition: root_dir_entries={root_dir_entries}, root_dir_sectors={root_dir_sectors}"
    );

    let first_data_sector = first_fat_sector + fat_size * table_count + root_dir_sectors;
    let data_sector_count = (total_sectors - first_data_sector) as u64;
    let data_cluster_count = data_sector_count / sectors_per_cluster;

    let mut fat = vec![0; (fat_size * sector_size) as usize];
    disk.read(start + first_fat_sector * sector_size, &mut fat)
        .await?;

    let mut chunks = Vec::new();
    chunks.push(ChunkInfo {
        start: 0,
        size: (first_data_sector * sector_size) as usize,
    });

    for idx in 2..2 + data_cluster_count as usize {
        if fat_type.index(&fat, idx) != 0 {
            chunks.push(ChunkInfo {
                start: (first_data_sector + (idx - 2) as u64 * sectors_per_cluster) as usize
                    * sector_size as usize,
                size: cluster_size as usize,
            });
        }
    }

    Ok(Some(chunks))
}
