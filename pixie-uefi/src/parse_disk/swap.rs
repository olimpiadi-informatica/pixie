use crate::{
    os::{disk::Disk, error::Result},
    store::ChunkInfo,
};
use alloc::vec::Vec;

pub async fn get_swap_chunks(disk: &Disk, start: u64, end: u64) -> Result<Option<Vec<ChunkInfo>>> {
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
