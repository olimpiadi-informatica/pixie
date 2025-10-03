use crate::{
    os::{disk::Disk, error::Result},
    store::ChunkInfo,
};
use alloc::vec::Vec;
use log::info;
use pixie_shared::util::BytesFmt;

pub async fn parse_gpt(disk: &mut Disk) -> Result<Option<Vec<ChunkInfo>>> {
    let disk_size = disk.size() as usize;
    let Ok(partitions) = disk.partitions() else {
        return Ok(None);
    };

    let mut pos = 0usize;
    let mut chunks = vec![];
    for partition in partitions {
        let begin = partition.byte_start as usize;
        let end = partition.byte_end as usize;
        info!(
            "Partition starting at 0x{begin:x}, size {}",
            BytesFmt((end - begin) as u64)
        );

        if pos < begin {
            chunks.push(ChunkInfo {
                start: pos,
                size: (begin - pos),
            });
        }

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
            start: pos,
            size: disk_size - pos,
        });
    }

    Ok(Some(chunks))
}
