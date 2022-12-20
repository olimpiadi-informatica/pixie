use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use gpt_disk_io::{
    gpt_disk_types::{BlockSize, Lba},
    BlockIo, DiskError,
};
use uefi::{
    proto::media::{block::BlockIO, disk::DiskIo},
    table::boot::{OpenProtocolParams, ScopedProtocol},
    Handle,
};

use super::{
    error::{Error, Result},
    UefiOS,
};

fn open_disk(
    os: UefiOS,
    handle: Handle,
) -> Result<(
    ScopedProtocol<'static, DiskIo>,
    ScopedProtocol<'static, BlockIO>,
)> {
    let bs = os.os().borrow().boot_services;
    let image_handle = os.os().borrow().boot_services.image_handle();
    let bio = unsafe {
        bs.open_protocol::<BlockIO>(
            OpenProtocolParams {
                agent: image_handle,
                controller: None,
                handle,
            },
            uefi::table::boot::OpenProtocolAttributes::GetProtocol,
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
    disk: ScopedProtocol<'static, DiskIo>,
    block: ScopedProtocol<'static, BlockIO>,
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
                return false;
            })
            .expect("Disk not found");

        let (disk, block) = open_disk(os, handle).unwrap();
        Disk { disk, block }
    }

    pub fn size(&self) -> u64 {
        self.block.media().block_size() as u64 * (self.block.media().last_block() + 1)
    }

    pub async fn flush(&mut self) {
        self.block.flush_blocks().unwrap();
    }

    pub async fn read(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        Ok(self
            .disk
            .read_disk(self.block.media().media_id(), offset, buf)?)
    }

    pub fn partitions(&mut self) -> Result<Vec<DiskPartition>> {
        let block_size = self.block_size().to_u64();
        let get_gpt_partitions = |d: &mut Disk| {
            let mut disk = gpt_disk_io::Disk::new(d)?;
            let mut buf = [0; 1 << 14];
            let header = disk.read_primary_gpt_header(&mut buf)?;
            // TODO(veluca): bubble up this error.
            let part_array_layout = header.get_partition_entry_array_layout().unwrap();
            let mut buf = [0; 1 << 14];
            let x = disk
                .gpt_partition_entry_array_iter(part_array_layout, &mut buf)?
                .filter_map(|part| {
                    let part = if let Err(err) = part {
                        return Some(Err(err));
                    } else {
                        part.unwrap()
                    };
                    if part.is_used() {
                        Some(Ok(DiskPartition {
                            byte_start: part.starting_lba.to_u64() * block_size,
                            byte_end: (part.ending_lba.to_u64() + 1) * block_size,
                            guid: part.unique_partition_guid.to_string(),
                            name: part.name.to_string(),
                        }))
                    } else {
                        None
                    }
                })
                .collect::<core::result::Result<Vec<_>, _>>();
            x
        };
        get_gpt_partitions(self).map_err(|e: DiskError<Error>| Error::Generic(e.to_string()))
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
        Ok(self
            .block
            .read_blocks(self.block.media().media_id(), start_lba.0, dst)?)
    }
    fn write_blocks(&mut self, start_lba: Lba, src: &[u8]) -> Result<()> {
        unreachable!();
    }
    fn flush(&mut self) -> Result<()> {
        Ok(self.block.flush_blocks()?)
    }
}
