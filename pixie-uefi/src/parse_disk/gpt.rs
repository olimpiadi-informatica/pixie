use alloc::vec::Vec;

use log::info;
use pixie_shared::util::BytesFmt;

use crate::os::disk::Disk;
use crate::os::error::Result;
use crate::store::ChunkInfo;

pub async fn parse_gpt(disk: &mut Disk) -> Result<Option<Vec<ChunkInfo>>> {
    let disk_size = disk.size() as usize;
    let (primary, secondary, mut partitions) = match disk.partitions() {
        Ok(partitions) => partitions,
        Err(e) => {
            log::debug!("Failed to parse GPT partitions: {e:?}");
            return Ok(None);
        }
    };
    partitions.sort_by_key(|p| p.byte_start);

    let mut pos = 0usize;
    let mut chunks = vec![ChunkInfo {
        start: 0,
        size: primary.1 as usize,
    }];
    for partition in partitions {
        let begin = partition.byte_start as usize;
        let end = partition.byte_end as usize;
        info!(
            "Partition starting at 0x{begin:x}, size {}",
            BytesFmt((end - begin) as u64)
        );

        let part_chunks = super::parse_partition(disk, begin as u64, end as u64).await?;
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
            start: secondary.0 as usize,
            size: secondary.1 as usize - secondary.0 as usize,
        });
    }

    Ok(Some(chunks))
}
