use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use gpt_disk_io::{
    gpt_disk_types::{BlockSize, Lba},
    BlockIo,
};
use uefi::{
    boot::ScopedProtocol,
    proto::media::{block::BlockIO, disk::DiskIo},
    boot::OpenProtocolParams,
    Handle,
};

use super::{
    error::{Error, Result},
    UefiOS,
};

fn open_disk(
    os: UefiOS,
    handle: Handle,
) -> Result<(ScopedProtocol<DiskIo>, ScopedProtocol<BlockIO>)> {
    let image_handle = uefi::boot::image_handle();
    let bio = unsafe {
        uefi::boot::open_protocol::<BlockIO>(
            OpenProtocolParams {
                agent: image_handle,
                controller: None,
                handle,
            },
            uefi::boot::OpenProtocolAttributes::GetProtocol,
        )
    };
    Ok((os.open_handle(handle)?, bio?))
}

#[derive(Debug)]
pub struct DiskPartition {
    pub byte_start: u64,
    pub byte_end: u64,
    pub guid: String,
    pub name: String,
}

pub struct Disk {
    disk: ScopedProtocol<DiskIo>,
    block: ScopedProtocol<BlockIO>,
    os: UefiOS,
}

// TODO(veluca): consider making parts of this actually async, i.e. by using DiskIo2/BlockIO2 if
// available; support having more than one disk.
impl Disk {
    pub fn new(os: UefiOS) -> Disk {
        let handle = os
            .all_handles::<DiskIo>()
            .unwrap()
            .into_iter()
            .find(|handle| {
                let op = open_disk(os, *handle);
                if let Ok((_, block)) = op {
                    if block.media().is_media_present() {
                        return true;
                    }
                }
                false
            })
            .expect("Disk not found");

        let (disk, block) = open_disk(os, handle).unwrap();
        Disk { disk, block, os }
    }

    pub fn size(&self) -> u64 {
        self.block.media().block_size() as u64 * (self.block.media().last_block() + 1)
    }

    pub async fn flush(&mut self) {
        self.block.flush_blocks().unwrap();
    }

    pub async fn read(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.os.schedule().await;
        Ok(self
            .disk
            .read_disk(self.block.media().media_id(), offset, buf)?)
    }

    pub async fn write(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        self.os.schedule().await;
        Ok(self
            .disk
            .write_disk(self.block.media().media_id(), offset, buf)?)
    }

    pub fn partitions(&mut self) -> Result<Vec<DiskPartition>> {
        let block_size = self.block_size().to_u64();
        let mut disk = gpt_disk_io::Disk::new(self).map_err(|e| Error::Generic(e.to_string()))?;
        let mut buf = [0; 1 << 14];
        let header = disk
            .read_primary_gpt_header(&mut buf)
            .map_err(|e| Error::Generic(e.to_string()))?;
        // TODO(veluca): bubble up this error.
        let part_array_layout = header
            .get_partition_entry_array_layout()
            .map_err(|e| Error::Generic(e.to_string()))?;
        let mut buf = [0; 1 << 14];
        let x = disk
            .gpt_partition_entry_array_iter(part_array_layout, &mut buf)
            .map_err(|e| Error::Generic(e.to_string()))?
            .filter_map(|part| {
                let part = if let Err(err) = part {
                    return Some(Err(err));
                } else {
                    part.unwrap()
                };
                if part.is_used() {
                    let part_guid = part.unique_partition_guid;
                    Some(Ok(DiskPartition {
                        byte_start: part.starting_lba.to_u64() * block_size,
                        byte_end: (part.ending_lba.to_u64() + 1) * block_size,
                        guid: part_guid.to_string(),
                        name: part.name.to_string(),
                    }))
                } else {
                    None
                }
            })
            .collect::<Result<_, _>>()
            .map_err(|e| Error::Generic(e.to_string()))?;

        Ok(x)
    }
}

impl gpt_disk_io::BlockIo for &mut Disk {
    type Error = super::error::Error;

    fn block_size(&self) -> BlockSize {
        BlockSize::new(self.block.media().block_size()).unwrap()
    }
    fn num_blocks(&mut self) -> Result<u64> {
        Ok(self.block.media().last_block() + 1)
    }
    fn read_blocks(&mut self, start_lba: Lba, dst: &mut [u8]) -> Result<()> {
        self.disk.read_disk(
            self.block.media().media_id(),
            self.block.media().block_size() as u64 * start_lba.0,
            dst,
        )?;
        Ok(())
    }
    fn write_blocks(&mut self, _start_lba: Lba, _src: &[u8]) -> Result<()> {
        unreachable!();
    }
    fn flush(&mut self) -> Result<()> {
        Ok(self.block.flush_blocks()?)
    }
}
