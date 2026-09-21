//! Fallback disk access through the firmware's `EFI_BLOCK_IO_PROTOCOL`.
//!
//! Used when no directly-addressable NVMe namespace is available, e.g. for
//! SATA/AHCI or USB mass-storage disks, or NVMe controllers exposed by
//! firmware only as a RAID/logical volume.

use alloc::vec;

use uefi::Handle;
use uefi::boot::{OpenProtocolParams, ScopedProtocol};
use uefi::proto::media::block::BlockIO;

use crate::os::error::Result;
use crate::os::executor::Executor;

fn open_disk(handle: Handle) -> Result<ScopedProtocol<BlockIO>> {
    let image_handle = uefi::boot::image_handle();
    let bio = unsafe {
        uefi::boot::open_protocol::<BlockIO>(
            OpenProtocolParams {
                agent: image_handle,
                controller: None,
                handle,
            },
            uefi::boot::OpenProtocolAttributes::GetProtocol,
        )?
    };
    Ok(bio)
}

pub struct BlockDisk {
    block: ScopedProtocol<BlockIO>,
}

impl BlockDisk {
    /// Opens the handle with the lowest `score(size)` among all present
    /// `EFI_BLOCK_IO_PROTOCOL` media, or `None` if none is present. Returns
    /// the winning score alongside the disk so callers can compare it
    /// against candidates from other backends.
    pub(super) fn choose(score: impl Fn(u64) -> u128) -> Option<(u128, BlockDisk)> {
        let (best_score, handle) = uefi::boot::find_handles::<BlockIO>()
            .unwrap()
            .into_iter()
            .filter_map(|handle| {
                let block = open_disk(handle).ok()?;
                let m = block.media();
                if !m.is_media_present() {
                    return None;
                }
                let size = (m.block_size() as u64).saturating_mul(m.last_block() + 1);
                Some((score(size), handle))
            })
            .min_by_key(|(score, _)| *score)?;

        let block = open_disk(handle).unwrap();
        Some((best_score, BlockDisk { block }))
    }

    pub(super) fn size(&self) -> u64 {
        self.block.media().block_size() as u64 * (self.block.media().last_block() + 1)
    }

    pub(super) fn block_size(&self) -> u32 {
        self.block.media().block_size()
    }

    pub(super) fn num_blocks(&self) -> u64 {
        self.block.media().last_block() + 1
    }

    pub(super) async fn flush(&mut self) -> Result<()> {
        self.block.flush_blocks()?;
        Ok(())
    }

    pub(super) fn read_sync(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let block_size = self.block.media().block_size() as u64;
        let media_id = self.block.media().media_id();
        let start_block = offset / block_size;
        let end_block = (offset + buf.len() as u64).div_ceil(block_size);
        let num_blocks = end_block - start_block;
        if buf.len() as u64 != num_blocks * block_size
            || !(buf.as_ptr() as usize).is_multiple_of(16)
        {
            let mut buf2 = vec![0u8; (num_blocks * block_size) as usize + 15];
            let delta = buf2.as_ptr().align_offset(16);
            let buf2 = &mut buf2[delta..delta + (num_blocks * block_size) as usize];
            self.block.read_blocks(media_id, start_block, buf2)?;
            let start_offset = (offset % block_size) as usize;
            buf.copy_from_slice(&buf2[start_offset..start_offset + buf.len()]);
        } else {
            self.block.read_blocks(media_id, start_block, buf)?;
        }
        Ok(())
    }

    pub(super) async fn read(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        Executor::sched_yield().await;
        self.read_sync(offset, buf)
    }

    pub(super) fn write_sync(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        let block_size = self.block.media().block_size() as u64;
        let media_id = self.block.media().media_id();
        let start_block = offset / block_size;
        let end_block = (offset + buf.len() as u64).div_ceil(block_size);
        let num_blocks = end_block - start_block;
        if buf.len() as u64 != num_blocks * block_size
            || !(buf.as_ptr() as usize).is_multiple_of(16)
        {
            let mut buf2 = vec![0u8; (num_blocks * block_size) as usize + 15];
            let delta = buf2.as_ptr().align_offset(16);
            let buf2 = &mut buf2[delta..delta + (num_blocks * block_size) as usize];
            self.block.read_blocks(media_id, start_block, buf2)?;
            let start_offset = (offset % block_size) as usize;
            buf2[start_offset..start_offset + buf.len()].copy_from_slice(buf);
            self.block.write_blocks(media_id, start_block, buf2)?;
        } else {
            self.block.write_blocks(media_id, start_block, buf)?;
        }
        Ok(())
    }

    pub(super) async fn write(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        Executor::sched_yield().await;
        self.write_sync(offset, buf)
    }
}
