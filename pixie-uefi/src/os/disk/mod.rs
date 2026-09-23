pub mod ahci;
pub mod nvme;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use gpt_disk_io::gpt_disk_types::{BlockSize, Lba};

pub use self::ahci::AhciDisk;
pub use self::nvme::NvmeDisk;
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
    Ahci(AhciDisk),
}

impl Disk {
    pub async fn largest() -> Disk {
        let nvme_disks = NvmeDisk::probe_all().await;
        let ahci_disks = AhciDisk::probe_all().await;

        let mut all: Vec<Disk> = Vec::new();
        for n in nvme_disks {
            all.push(Disk::Nvme(n));
        }
        for a in ahci_disks {
            all.push(Disk::Ahci(a));
        }

        all.into_iter()
            .max_by_key(|d| d.size())
            .expect("No NVMe or SATA disk found")
    }

    pub async fn open_with_size(size: u64) -> Disk {
        let nvme_disks = NvmeDisk::probe_all().await;
        let ahci_disks = AhciDisk::probe_all().await;

        let mut all: Vec<Disk> = Vec::new();
        for n in nvme_disks {
            all.push(Disk::Nvme(n));
        }
        for a in ahci_disks {
            all.push(Disk::Ahci(a));
        }

        all.into_iter()
            .find(|d| d.size() == size)
            .expect("No disk with requested size found")
    }

    pub fn size(&self) -> u64 {
        match self {
            Disk::Nvme(d) => d.size(),
            Disk::Ahci(d) => d.size(),
        }
    }

    pub fn block_size_raw(&self) -> u32 {
        match self {
            Disk::Nvme(d) => d.block_size(),
            Disk::Ahci(d) => d.block_size(),
        }
    }

    pub async fn flush(&mut self) -> Result<()> {
        match self {
            Disk::Nvme(d) => d.flush().await,
            Disk::Ahci(d) => d.flush().await,
        }
    }

    pub fn read_sync(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        match self {
            Disk::Nvme(d) => d.read_sync(offset, buf),
            Disk::Ahci(d) => d.read_sync(offset, buf),
        }
    }

    pub async fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        match self {
            Disk::Nvme(d) => d.read(offset, buf).await,
            Disk::Ahci(d) => d.read(offset, buf).await,
        }
    }

    pub fn write_sync(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        match self {
            Disk::Nvme(d) => d.write_sync(offset, buf),
            Disk::Ahci(d) => d.write_sync(offset, buf),
        }
    }

    pub async fn write(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        match self {
            Disk::Nvme(d) => d.write(offset, buf).await,
            Disk::Ahci(d) => d.write(offset, buf).await,
        }
    }

    #[allow(clippy::type_complexity)]
    pub fn partitions(&mut self) -> Result<((u64, u64), (u64, u64), Vec<DiskPartition>)> {
        fn gpt_range(
            header: &gpt_disk_io::gpt_disk_types::GptHeader,
            block_size: u64,
        ) -> (u64, u64) {
            let header_start = header.my_lba.to_u64() * block_size;
            let header_end = header_start + block_size;

            let entries_bytes = header.number_of_partition_entries.to_u32() as u64
                * header.size_of_partition_entry.to_u32() as u64;
            let entries_blocks = entries_bytes.div_ceil(block_size);
            let entries_start = header.partition_entry_lba.to_u64() * block_size;
            let entries_end = entries_start + entries_blocks * block_size;

            (header_start.min(entries_start), header_end.max(entries_end))
        }

        let block_size = self.block_size_raw() as u64;
        let mut disk = gpt_disk_io::Disk::new(self)?;
        let mut buf = [0; 1 << 14];
        let header = disk.read_primary_gpt_header(&mut buf)?;
        let secondary_header = disk.read_secondary_gpt_header(&mut buf)?;
        let part_array_layout = header.get_partition_entry_array_layout()?;
        let mut buf = [0; 1 << 14];
        let x = disk
            .gpt_partition_entry_array_iter(part_array_layout, &mut buf)?
            .filter_map(|part| {
                let part = match part {
                    Ok(part) => part,
                    Err(err) => return Some(Err(err)),
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
            .collect::<Result<_, _>>()?;

        Ok((
            gpt_range(&header, block_size),
            gpt_range(&secondary_header, block_size),
            x,
        ))
    }
}

impl gpt_disk_io::BlockIo for &mut Disk {
    type Error = super::error::Error;

    fn block_size(&self) -> BlockSize {
        BlockSize::new(self.block_size_raw()).unwrap()
    }

    fn num_blocks(&mut self) -> Result<u64> {
        Ok(match self {
            Disk::Nvme(d) => d.num_blocks(),
            Disk::Ahci(d) => d.num_blocks(),
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
