use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::future::Future;

use gpt_disk_io::gpt_disk_types::{BlockSize, GptHeader, Lba};

use crate::error::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskPartition {
    pub byte_start: u64,
    pub byte_end: u64,
    pub guid: String,
    pub name: String,
}

/// Async block/byte disk interface used by `pixie-client` (both on bare metal and in host tests).
#[allow(clippy::type_complexity)]
pub trait DiskIo {
    fn size(&self) -> u64;
    fn block_size(&self) -> u32 {
        512
    }
    fn read(&mut self, offset: u64, buf: &mut [u8]) -> impl Future<Output = Result<()>>;
    fn write(&mut self, offset: u64, buf: &[u8]) -> impl Future<Output = Result<()>>;
    fn flush(&mut self) -> impl Future<Output = Result<()>>;
    fn partitions(
        &mut self,
    ) -> impl Future<Output = Result<((u64, u64), (u64, u64), Vec<DiskPartition>)>> {
        read_gpt_partitions(self)
    }
}

/// Asynchronously reads and validates the primary and secondary GPT headers and partition entry array
/// from any `D: DiskIo` without synchronous block I/O.
pub async fn read_gpt_partitions<D: DiskIo + ?Sized>(
    disk: &mut D,
) -> Result<((u64, u64), (u64, u64), Vec<DiskPartition>)> {
    fn gpt_range(header: &GptHeader, block_size: u64) -> (u64, u64) {
        let header_start = header.my_lba.to_u64() * block_size;
        let header_end = header_start + block_size;

        let entries_bytes = header.number_of_partition_entries.to_u32() as u64
            * header.size_of_partition_entry.to_u32() as u64;
        let entries_blocks = entries_bytes.div_ceil(block_size);
        let entries_start = header.partition_entry_lba.to_u64() * block_size;
        let entries_end = entries_start + entries_blocks * block_size;

        (header_start.min(entries_start), header_end.max(entries_end))
    }

    let bs = disk.block_size() as u64;
    let total_size = disk.size();
    if bs == 0 || total_size < bs * 3 {
        return Err(Error::msg("Disk too small for GPT"));
    }
    let num_blocks = total_size / bs;

    // Pre-fetch primary GPT region (first 64 KiB or up to disk size) and secondary GPT region (last 64 KiB)
    let window = (64 * 1024).min(total_size as usize);
    let mut head_buf = vec![0u8; window];
    disk.read(0, &mut head_buf).await?;

    let tail_start = total_size.saturating_sub(window as u64) & !(bs - 1);
    let tail_len = (total_size - tail_start) as usize;
    let mut tail_buf = vec![0u8; tail_len];
    disk.read(tail_start, &mut tail_buf).await?;

    struct CachedGptIo<'a> {
        bs: u32,
        num_blocks: u64,
        head_buf: &'a [u8],
        tail_start: u64,
        tail_buf: &'a [u8],
    }

    impl gpt_disk_io::BlockIo for CachedGptIo<'_> {
        type Error = Error;

        fn block_size(&self) -> BlockSize {
            BlockSize::new(self.bs).unwrap()
        }

        fn num_blocks(&mut self) -> Result<u64> {
            Ok(self.num_blocks)
        }

        fn read_blocks(&mut self, start_lba: Lba, dst: &mut [u8]) -> Result<()> {
            let offset = start_lba.to_u64() * (self.bs as u64);
            let end = offset + dst.len() as u64;
            if end <= self.head_buf.len() as u64 {
                dst.copy_from_slice(&self.head_buf[offset as usize..end as usize]);
                Ok(())
            } else if offset >= self.tail_start
                && end <= self.tail_start + self.tail_buf.len() as u64
            {
                let rel = (offset - self.tail_start) as usize;
                dst.copy_from_slice(&self.tail_buf[rel..rel + dst.len()]);
                Ok(())
            } else {
                Err(Error::msg("GPT metadata outside cached head/tail window"))
            }
        }

        fn write_blocks(&mut self, _start_lba: Lba, _src: &[u8]) -> Result<()> {
            unreachable!()
        }

        fn flush(&mut self) -> Result<()> {
            Ok(())
        }
    }

    let io = CachedGptIo {
        bs: disk.block_size(),
        num_blocks,
        head_buf: &head_buf,
        tail_start,
        tail_buf: &tail_buf,
    };

    let mut gpt_disk = gpt_disk_io::Disk::new(io)?;
    let mut buf = [0u8; 1 << 14];
    let header = gpt_disk.read_primary_gpt_header(&mut buf)?;
    let secondary_header = gpt_disk.read_secondary_gpt_header(&mut buf)?;
    let part_array_layout = header.get_partition_entry_array_layout()?;
    let mut buf = [0u8; 1 << 14];
    let partitions = gpt_disk
        .gpt_partition_entry_array_iter(part_array_layout, &mut buf)?
        .filter_map(|part| match part {
            Err(err) => Some(Err(Error::from(err))),
            Ok(part) => {
                if part.is_used() {
                    let guid = part.unique_partition_guid;
                    let name = part.name;
                    Some(Ok(DiskPartition {
                        byte_start: part.starting_lba.to_u64() * bs,
                        byte_end: (part.ending_lba.to_u64() + 1) * bs,
                        guid: guid.to_string(),
                        name: name.to_string(),
                    }))
                } else {
                    None
                }
            }
        })
        .collect::<Result<Vec<_>>>()?;

    Ok((
        gpt_range(&header, bs),
        gpt_range(&secondary_header, bs),
        partitions,
    ))
}

enum KernelBlockDevice {
    Nvme(pixie_drivers::disk::nvme::NvmeDisk<crate::hal::NativeHal>),
    Ahci(pixie_drivers::disk::ahci::AhciDisk<crate::hal::NativeHal>),
}

impl KernelBlockDevice {
    async fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        match self {
            Self::Nvme(d) => d.read(offset, buf).await.map_err(Error::from),
            Self::Ahci(d) => d.read(offset, buf).await.map_err(Error::from),
        }
    }

    async fn write(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        match self {
            Self::Nvme(d) => d.write(offset, buf).await.map_err(Error::from),
            Self::Ahci(d) => d.write(offset, buf).await.map_err(Error::from),
        }
    }

    async fn flush(&mut self) -> Result<()> {
        match self {
            Self::Nvme(d) => d.flush().await.map_err(Error::from),
            Self::Ahci(d) => d.flush().await.map_err(Error::from),
        }
    }
}

struct DiskEntry {
    size: u64,
    block_size: u32,
    dev: Option<KernelBlockDevice>,
}

static DISCOVERED_DISKS: spin::Mutex<Vec<DiskEntry>> = spin::Mutex::new(Vec::new());
static DISKS_INITIALIZED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

pub struct Disk {
    index: usize,
    size: u64,
    block_size: u32,
}

impl Disk {
    pub async fn init() {
        if DISKS_INITIALIZED.swap(true, core::sync::atomic::Ordering::SeqCst) {
            return;
        }

        let hal = crate::hal::NativeHal;
        let pci_devs = pixie_drivers::pci::scan_pci(&hal);
        let mut entries = Vec::new();

        for &dev in &pci_devs {
            // NVMe controller: class 0x01, subclass 0x08
            if dev.class_code == 0x01 && dev.subclass == 0x08 {
                dev.enable_bus_mastering(&hal);
                if let Some(pixie_drivers::pci::Bar::Memory { base: bar0, .. }) =
                    dev.read_bar(&hal, 0)
                {
                    crate::arch::paging::map_mmio(bar0, 0x10000);
                    match pixie_drivers::disk::nvme::NvmeDisk::open(dev, bar0, hal).await {
                        Ok(d) => {
                            let size = d.size();
                            let block_size = d.block_size();
                            log::info!(
                                "Initialized NVMe namespace {} ({} bytes, block_size={})",
                                d.namespace(),
                                size,
                                block_size
                            );
                            entries.push(DiskEntry {
                                size,
                                block_size,
                                dev: Some(KernelBlockDevice::Nvme(d)),
                            });
                        }
                        Err(e) => {
                            log::warn!("NVMe init failed at BAR0={:#x}: {}", bar0, e);
                        }
                    }
                }
            }
        }

        for &dev in &pci_devs {
            // AHCI SATA controller: class 0x01, subclass 0x06
            if dev.class_code == 0x01 && dev.subclass == 0x06 {
                dev.enable_bus_mastering(&hal);
                if let Some(pixie_drivers::pci::Bar::Memory { base: abar, .. }) =
                    dev.read_bar(&hal, 5)
                {
                    crate::arch::paging::map_mmio(abar, 0x10000);
                    let ports =
                        pixie_drivers::disk::ahci::AhciDisk::<crate::hal::NativeHal>::probe_sata_ports(
                            abar, &hal,
                        );
                    for port in ports {
                        match pixie_drivers::disk::ahci::AhciDisk::open(dev, abar, port, hal).await
                        {
                            Ok(d) => {
                                let size = d.size();
                                let block_size = d.block_size();
                                log::info!(
                                    "Initialized AHCI port {} ({} bytes, block_size={})",
                                    port,
                                    size,
                                    block_size
                                );
                                entries.push(DiskEntry {
                                    size,
                                    block_size,
                                    dev: Some(KernelBlockDevice::Ahci(d)),
                                });
                            }
                            Err(e) => {
                                log::warn!("AHCI port {} init failed: {}", port, e);
                            }
                        }
                    }
                }
            }
        }

        *DISCOVERED_DISKS.lock() = entries;
    }

    pub async fn largest() -> Self {
        if !DISKS_INITIALIZED.load(core::sync::atomic::Ordering::SeqCst) {
            Self::init().await;
        }
        let disks = DISCOVERED_DISKS.lock();
        let (idx, entry) = disks
            .iter()
            .enumerate()
            .max_by_key(|(_, e)| e.size)
            .expect("No disks found");
        Self {
            index: idx,
            size: entry.size,
            block_size: entry.block_size,
        }
    }

    pub async fn open_with_size(base_size: u64) -> Self {
        if !DISKS_INITIALIZED.load(core::sync::atomic::Ordering::SeqCst) {
            Self::init().await;
        }
        let disks = DISCOVERED_DISKS.lock();
        let (idx, entry) = disks
            .iter()
            .enumerate()
            .find(|(_, e)| e.size == base_size)
            .or_else(|| disks.iter().enumerate().max_by_key(|(_, e)| e.size))
            .expect("No disk matching requested size");
        Self {
            index: idx,
            size: entry.size,
            block_size: entry.block_size,
        }
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    pub async fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let mut dev = {
            let mut disks = DISCOVERED_DISKS.lock();
            disks
                .get_mut(self.index)
                .and_then(|e| e.dev.take())
                .ok_or_else(|| Error::msg("Disk device busy or unavailable"))?
        };
        let res = dev.read(offset, buf).await;
        DISCOVERED_DISKS.lock()[self.index].dev = Some(dev);
        res
    }

    pub async fn write(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        let mut dev = {
            let mut disks = DISCOVERED_DISKS.lock();
            disks
                .get_mut(self.index)
                .and_then(|e| e.dev.take())
                .ok_or_else(|| Error::msg("Disk device busy or unavailable"))?
        };
        let res = dev.write(offset, buf).await;
        DISCOVERED_DISKS.lock()[self.index].dev = Some(dev);
        res
    }

    pub async fn flush(&mut self) -> Result<()> {
        let mut dev = {
            let mut disks = DISCOVERED_DISKS.lock();
            disks
                .get_mut(self.index)
                .and_then(|e| e.dev.take())
                .ok_or_else(|| Error::msg("Disk device busy or unavailable"))?
        };
        let res = dev.flush().await;
        DISCOVERED_DISKS.lock()[self.index].dev = Some(dev);
        res
    }

    pub async fn partitions(&mut self) -> Result<((u64, u64), (u64, u64), Vec<DiskPartition>)> {
        read_gpt_partitions(self).await
    }
}

impl DiskIo for Disk {
    fn size(&self) -> u64 {
        Disk::size(self)
    }

    fn block_size(&self) -> u32 {
        Disk::block_size(self)
    }

    async fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        Disk::read(self, offset, buf).await
    }

    async fn write(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        Disk::write(self, offset, buf).await
    }

    async fn flush(&mut self) -> Result<()> {
        Disk::flush(self).await
    }
}
