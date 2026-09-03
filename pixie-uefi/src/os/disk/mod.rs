//! Disk access, choosing by size across both a directly-addressable NVMe
//! namespace and the firmware's `EFI_BLOCK_IO_PROTOCOL` (SATA/AHCI, USB, or
//! an NVMe controller only exposed as a logical volume).

mod block;
mod nvme;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use gpt_disk_io::BlockIo;
use gpt_disk_io::gpt_disk_types::{BlockSize, Lba};

use self::block::BlockDisk;
use self::nvme::NvmeDisk;
use super::error::Result;

#[derive(Debug)]
pub struct DiskPartition {
    pub byte_start: u64,
    pub byte_end: u64,
    pub guid: String,
    pub name: String,
}

pub enum Disk {
    Nvme(NvmeDisk),
    Block(BlockDisk),
}

impl Disk {
    /// Opens the disk with the lowest `score(size)` among both NVMe
    /// namespaces and `EFI_BLOCK_IO_PROTOCOL` media, so a good NVMe match
    /// doesn't shadow a better-matching disk only reachable via BlockIO (or
    /// vice versa).
    fn choose(score: impl Fn(u64) -> u128 + Copy) -> Disk {
        let nvme = NvmeDisk::choose(score);
        let block = BlockDisk::choose(score);
        match (nvme, block) {
            (Some((nvme_score, nvme)), Some((block_score, _))) if nvme_score <= block_score => {
                Disk::Nvme(nvme)
            }
            (_, Some((_, block))) => Disk::Block(block),
            (Some((_, nvme)), None) => Disk::Nvme(nvme),
            (None, None) => panic!("Disk not found"),
        }
    }

    pub fn largest() -> Disk {
        Self::choose(|size| u128::MAX - size as u128)
    }

    #[cfg(feature = "coverage")]
    pub fn open_with_size(base_size: i64) -> Disk {
        Self::choose(move |size| (size as i128 - base_size as i128).unsigned_abs())
    }

    pub fn size(&self) -> u64 {
        match self {
            Disk::Nvme(disk) => disk.size(),
            Disk::Block(disk) => disk.size(),
        }
    }

    pub async fn flush(&mut self) -> Result<()> {
        match self {
            Disk::Nvme(disk) => disk.flush().await,
            Disk::Block(disk) => disk.flush().await,
        }
    }

    pub fn read_sync(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        match self {
            Disk::Nvme(disk) => disk.read_sync(offset, buf),
            Disk::Block(disk) => disk.read_sync(offset, buf),
        }
    }

    pub async fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        match self {
            Disk::Nvme(disk) => disk.read(offset, buf).await,
            Disk::Block(disk) => disk.read(offset, buf).await,
        }
    }

    pub fn write_sync(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        match self {
            Disk::Nvme(disk) => disk.write_sync(offset, buf),
            Disk::Block(disk) => disk.write_sync(offset, buf),
        }
    }

    pub async fn write(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        match self {
            Disk::Nvme(disk) => disk.write(offset, buf).await,
            Disk::Block(disk) => disk.write(offset, buf).await,
        }
    }

    fn block_size_raw(&self) -> u32 {
        match self {
            Disk::Nvme(disk) => disk.block_size(),
            Disk::Block(disk) => disk.block_size(),
        }
    }

    pub fn partitions(&mut self) -> Result<Vec<DiskPartition>> {
        let block_size = self.block_size().to_u64();
        let mut disk = gpt_disk_io::Disk::new(self)?;
        let mut buf = [0; 1 << 14];
        let header = disk.read_primary_gpt_header(&mut buf)?;
        let part_array_layout = header.get_partition_entry_array_layout()?;
        let mut buf = [0; 1 << 14];
        Ok(disk
            .gpt_partition_entry_array_iter(part_array_layout, &mut buf)?
            .filter_map(|part| {
                let part = match part {
                    Ok(part) => part,
                    Err(error) => return Some(Err(error)),
                };
                part.is_used().then(|| {
                    let part_guid = part.unique_partition_guid;
                    Ok(DiskPartition {
                        byte_start: part.starting_lba.to_u64() * block_size,
                        byte_end: (part.ending_lba.to_u64() + 1) * block_size,
                        guid: part_guid.to_string(),
                        name: part.name.to_string(),
                    })
                })
            })
            .collect::<Result<_, _>>()?)
    }
}

impl gpt_disk_io::BlockIo for &mut Disk {
    type Error = super::error::Error;

    fn block_size(&self) -> BlockSize {
        BlockSize::new(self.block_size_raw()).unwrap()
    }

    fn num_blocks(&mut self) -> Result<u64> {
        Ok(match self {
            Disk::Nvme(disk) => disk.num_blocks(),
            Disk::Block(disk) => disk.num_blocks(),
        })
    }

    fn read_blocks(&mut self, start_lba: Lba, dst: &mut [u8]) -> Result<()> {
        let block_size = self.block_size_raw() as u64;
        self.read_sync(block_size * start_lba.0, dst)
    }

    fn write_blocks(&mut self, _start_lba: Lba, _src: &[u8]) -> Result<()> {
        unreachable!()
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}
