pub mod ahci;
pub mod nvme;

use crate::error::{Error, Result};

pub(crate) trait BlockDeviceIo {
    fn total_bytes(&self) -> u64;
    fn block_bytes(&self) -> usize;
    fn max_transfer_blocks(&self) -> usize;
    fn dma_slice(&self) -> &[u8];
    fn dma_mut_slice(&mut self) -> &mut [u8];
    async fn transfer_blocks(
        &mut self,
        write: bool,
        lba: u64,
        blocks: u32,
        transfer_bytes: usize,
    ) -> Result<()>;
}

pub(crate) async fn read_bytes<D: BlockDeviceIo>(
    dev: &mut D,
    offset: u64,
    buf: &mut [u8],
    overflow_err: &'static str,
    oob_err: &'static str,
) -> Result<()> {
    if buf.is_empty() {
        return Ok(());
    }
    let end = offset
        .checked_add(buf.len() as u64)
        .ok_or_else(|| Error::msg(overflow_err))?;
    if end > dev.total_bytes() {
        return Err(Error::msg(oob_err));
    }

    let bs = dev.block_bytes();
    let bs_u64 = bs as u64;
    let max_blocks = dev.max_transfer_blocks();

    let mut cur_offset = offset;
    let mut remaining = buf;
    while !remaining.is_empty() {
        let lba = cur_offset / bs_u64;
        let in_block = (cur_offset % bs_u64) as usize;
        let blocks = (in_block + remaining.len()).div_ceil(bs).min(max_blocks);
        let transfer_bytes = blocks * bs;

        dev.transfer_blocks(false, lba, blocks as u32, transfer_bytes)
            .await?;
        let chunk_bytes = (transfer_bytes - in_block).min(remaining.len());
        remaining[..chunk_bytes]
            .copy_from_slice(&dev.dma_slice()[in_block..in_block + chunk_bytes]);

        cur_offset += chunk_bytes as u64;
        remaining = &mut remaining[chunk_bytes..];
    }
    Ok(())
}

pub(crate) async fn write_bytes<D: BlockDeviceIo>(
    dev: &mut D,
    offset: u64,
    buf: &[u8],
    overflow_err: &'static str,
    oob_err: &'static str,
) -> Result<()> {
    if buf.is_empty() {
        return Ok(());
    }
    let end = offset
        .checked_add(buf.len() as u64)
        .ok_or_else(|| Error::msg(overflow_err))?;
    if end > dev.total_bytes() {
        return Err(Error::msg(oob_err));
    }

    let bs = dev.block_bytes();
    let bs_u64 = bs as u64;
    let max_blocks = dev.max_transfer_blocks();

    let mut cur_offset = offset;
    let mut remaining = buf;
    while !remaining.is_empty() {
        let lba = cur_offset / bs_u64;
        let in_block = (cur_offset % bs_u64) as usize;
        let blocks = (in_block + remaining.len()).div_ceil(bs).min(max_blocks);
        let transfer_bytes = blocks * bs;
        let chunk_bytes = (transfer_bytes - in_block).min(remaining.len());

        if in_block != 0 || chunk_bytes < transfer_bytes {
            dev.transfer_blocks(false, lba, blocks as u32, transfer_bytes)
                .await?;
        }

        dev.dma_mut_slice()[in_block..in_block + chunk_bytes]
            .copy_from_slice(&remaining[..chunk_bytes]);

        dev.transfer_blocks(true, lba, blocks as u32, transfer_bytes)
            .await?;

        cur_offset += chunk_bytes as u64;
        remaining = &remaining[chunk_bytes..];
    }
    Ok(())
}
