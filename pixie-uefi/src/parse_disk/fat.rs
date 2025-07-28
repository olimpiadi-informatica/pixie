use super::{le16, le32};
use crate::{
    os::{disk::Disk, error::Result},
    store::ChunkInfo,
};
use alloc::vec::Vec;

pub async fn get_fat_chunks(disk: &Disk, start: u64, end: u64) -> Result<Option<Vec<ChunkInfo>>> {
    if end - start < 512 {
        return Ok(None);
    }

    let mut ebpb = [0; 512];
    disk.read(start, &mut ebpb).await?;
    if &ebpb[0x52..0x5A] != b"FAT32   " {
        return Ok(None); // Not a FAT32 partition
    }

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
    let fat_size = le32(&ebpb, 0x24) as u64;
    let table_count = ebpb[0x10] as u64;
    log::trace!(
        "FAT partition: first_fat_sector={first_fat_sector}, fat_size={fat_size}, table_count={table_count}"
    );

    let first_data_sector = first_fat_sector + fat_size * table_count;
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

    for idx in 0..data_cluster_count as usize {
        let val = le32(&fat, idx * 4);
        if val != 0 {
            chunks.push(ChunkInfo {
                start: (first_data_sector + idx as u64 * sectors_per_cluster) as usize
                    * sector_size as usize,
                size: cluster_size as usize,
            });
        }
    }

    Ok(Some(chunks))
}
