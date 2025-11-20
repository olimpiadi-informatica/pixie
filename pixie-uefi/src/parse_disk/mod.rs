use crate::{
    os::{disk::Disk, error::Result},
    store::ChunkInfo,
};
use alloc::vec::Vec;
use log::info;
use pixie_shared::{util::BytesFmt, MAX_CHUNK_SIZE};

mod ext4;
mod fat;
mod gpt;
mod ntfs;
mod swap;

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

/// Returns chunks *relative to the start of the partition*.
async fn parse_partition(disk: &Disk, start: u64, end: u64) -> Result<Vec<ChunkInfo>> {
    if let Some(chunks) = fat::get_fat_chunks(disk, start, end).await? {
        info!(
            "FAT partition with {} chunks of size {}",
            chunks.len(),
            BytesFmt(chunks.iter().map(|x| x.size as u64).sum::<u64>())
        );
        Ok(chunks)
    } else if let Some(chunks) = ext4::get_ext4_chunks(disk, start, end).await? {
        info!(
            "Ext4 partition with {} chunks of size {}",
            chunks.len(),
            BytesFmt(chunks.iter().map(|x| x.size as u64).sum::<u64>())
        );
        Ok(chunks)
    } else if let Some(chunks) = ntfs::get_ntfs_chunks(disk, start, end).await? {
        info!(
            "NTFS partition with {} chunks of size {}",
            chunks.len(),
            BytesFmt(chunks.iter().map(|x| x.size as u64).sum::<u64>())
        );
        Ok(chunks)
    } else if let Some(chunks) = swap::get_swap_chunks(disk, start, end).await? {
        info!(
            "Swap partition with {} chunks of size {}",
            chunks.len(),
            BytesFmt(chunks.iter().map(|x| x.size as u64).sum::<u64>())
        );
        Ok(chunks)
    } else {
        info!("Unknown partition type");
        Ok(vec![ChunkInfo {
            start: 0,
            size: (end - start) as usize,
        }])
    }
}

async fn parse_partition_table(disk: &mut Disk) -> Result<Vec<ChunkInfo>> {
    if let Some(chunks) = gpt::parse_gpt(disk).await? {
        Ok(chunks)
    } else {
        parse_partition(disk, 0, disk.size()).await
    }
}

pub async fn parse_disk(disk: &mut Disk) -> Result<Vec<ChunkInfo>> {
    let chunks = parse_partition_table(disk).await?;

    let ChunkInfo { start, size } = chunks.last().unwrap();
    let last_end = start + size;
    const MIN_CHUNK_SIZE: usize = 1<<16;
    let mut last_chunk_end = 0;
    // Split up chunks.
    let mut final_chunks = Vec::<ChunkInfo>::new();
    for ChunkInfo { mut start, size } in chunks {
        let mut end = start + size;
        if end <= last_chunk_end {
            // os.append_message(format!("{} {}", end, last_chunk_end), MessageKind::Warning);
            continue;
        }
        start = start / MIN_CHUNK_SIZE * MIN_CHUNK_SIZE;
        end = end.next_multiple_of(MIN_CHUNK_SIZE).min(last_end);
        if start < last_chunk_end {
            start = last_chunk_end;
        }
        last_chunk_end = end;

        if let Some(last) = final_chunks.last() {
            assert!(last.start + last.size <= start);
            if last.start + last.size == start {
                start = last.start;
                final_chunks.pop();
            }
        }

        while start < end {
            let split = (start + 1).next_multiple_of(MAX_CHUNK_SIZE).min(end);
            final_chunks.push(ChunkInfo {
                start,
                size: split - start,
            });
            start = split;
        }
    }
    Ok(final_chunks)
}
