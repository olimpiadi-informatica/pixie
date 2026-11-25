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
    fn gpt_range(header: &GptHeader, block_size: u64) -> Result<(u64, u64)> {
        let header_start = header
            .my_lba
            .to_u64()
            .checked_mul(block_size)
            .ok_or(Error::msg("GPT header offset overflow"))?;
        let header_end = header_start
            .checked_add(block_size)
            .ok_or(Error::msg("GPT header end overflow"))?;

        let entries_bytes = (header.number_of_partition_entries.to_u32() as u64)
            .checked_mul(header.size_of_partition_entry.to_u32() as u64)
            .ok_or(Error::msg("GPT partition array size overflow"))?;
        let entries_blocks = entries_bytes.div_ceil(block_size);
        let entries_start = header
            .partition_entry_lba
            .to_u64()
            .checked_mul(block_size)
            .ok_or(Error::msg("GPT entries offset overflow"))?;
        let entries_span = entries_blocks
            .checked_mul(block_size)
            .ok_or(Error::msg("GPT entries span overflow"))?;
        let entries_end = entries_start
            .checked_add(entries_span)
            .ok_or(Error::msg("GPT entries end overflow"))?;

        Ok((header_start.min(entries_start), header_end.max(entries_end)))
    }

    fn validate_raw_header_basics(
        hdr_bytes: &[u8],
        bs: u64,
        num_blocks: u64,
    ) -> Result<(u64, usize)> {
        if hdr_bytes.len() < 92 || &hdr_bytes[0..8] != b"EFI PART" {
            return Err(Error::msg("Invalid GPT header signature"));
        }
        let hdr_size = u32::from_le_bytes(hdr_bytes[12..16].try_into().unwrap()) as u64;
        if hdr_size < 92 || hdr_size > bs {
            return Err(Error::msg("Invalid GPT header size"));
        }
        let entries_lba = u64::from_le_bytes(hdr_bytes[72..80].try_into().unwrap());
        let entry_count = u32::from_le_bytes(hdr_bytes[80..84].try_into().unwrap()) as u64;
        let entry_size = u32::from_le_bytes(hdr_bytes[84..88].try_into().unwrap()) as u64;
        if entry_count == 0 || entry_size < 128 || !entry_size.is_power_of_two() {
            return Err(Error::msg("Invalid GPT partition entry layout"));
        }
        let entries_bytes = entry_count
            .checked_mul(entry_size)
            .ok_or(Error::msg("GPT entries size overflow"))?;
        if entries_bytes > 1024 * 1024 {
            return Err(Error::msg("GPT partition table exceeds 1 MiB limit"));
        }
        let entries_blocks = entries_bytes.div_ceil(bs);
        let entries_end_lba = entries_lba
            .checked_add(entries_blocks)
            .ok_or(Error::msg("GPT entries LBA overflow"))?;
        if entries_end_lba > num_blocks {
            return Err(Error::msg("GPT partition array exceeds disk size"));
        }
        let start_off = entries_lba
            .checked_mul(bs)
            .ok_or(Error::msg("GPT entries byte offset overflow"))?;
        let aligned_len = (entries_blocks
            .checked_mul(bs)
            .ok_or(Error::msg("GPT entries aligned length overflow"))?)
            as usize;
        Ok((start_off, aligned_len))
    }

    let bs_u32 = disk.block_size();
    let bs = bs_u32 as u64;
    let total_size = disk.size();
    if bs < 512 || !bs.is_power_of_two() || total_size < bs * 3 {
        return Err(Error::msg("Disk too small or invalid block size for GPT"));
    }
    let num_blocks = total_size / bs;

    // 1. Read exact LBA 1 (primary GPT header) and parse/validate basics
    let mut pri_hdr_buf = vec![0u8; bs as usize];
    disk.read(bs, &mut pri_hdr_buf).await?;
    let (pri_entries_off, pri_entries_len) =
        validate_raw_header_basics(&pri_hdr_buf, bs, num_blocks)?;

    // 2. Read exact primary partition entry array based on header fields
    let mut pri_entries_buf = vec![0u8; pri_entries_len];
    disk.read(pri_entries_off, &mut pri_entries_buf).await?;

    // 3. Read exact secondary GPT header at last LBA (and secondary entries)
    let sec_hdr_off = (num_blocks - 1) * bs;
    let mut sec_hdr_buf = vec![0u8; bs as usize];
    disk.read(sec_hdr_off, &mut sec_hdr_buf).await?;
    let (sec_entries_off, sec_entries_len) =
        validate_raw_header_basics(&sec_hdr_buf, bs, num_blocks)?;
    let mut sec_entries_buf = vec![0u8; sec_entries_len];
    disk.read(sec_entries_off, &mut sec_entries_buf).await?;

    struct ExactGptIo<'a> {
        bs: u32,
        num_blocks: u64,
        slices: [(u64, &'a [u8]); 4],
    }

    impl gpt_disk_io::BlockIo for ExactGptIo<'_> {
        type Error = Error;

        fn block_size(&self) -> BlockSize {
            BlockSize::new(self.bs).unwrap()
        }

        fn num_blocks(&mut self) -> Result<u64> {
            Ok(self.num_blocks)
        }

        fn read_blocks(&mut self, start_lba: Lba, dst: &mut [u8]) -> Result<()> {
            let offset = start_lba
                .to_u64()
                .checked_mul(self.bs as u64)
                .ok_or(Error::msg("GPT LBA offset overflow"))?;
            let end = offset
                .checked_add(dst.len() as u64)
                .ok_or(Error::msg("GPT read length overflow"))?;
            for &(base, buf) in &self.slices {
                if offset >= base && end <= base.saturating_add(buf.len() as u64) {
                    let rel = (offset - base) as usize;
                    dst.copy_from_slice(&buf[rel..rel + dst.len()]);
                    return Ok(());
                }
            }
            Err(Error::msg("GPT metadata block outside fetched ranges"))
        }

        fn write_blocks(&mut self, _start_lba: Lba, _src: &[u8]) -> Result<()> {
            unreachable!()
        }

        fn flush(&mut self) -> Result<()> {
            Ok(())
        }
    }

    let io = ExactGptIo {
        bs: bs_u32,
        num_blocks,
        slices: [
            (bs, &pri_hdr_buf),
            (pri_entries_off, &pri_entries_buf),
            (sec_hdr_off, &sec_hdr_buf),
            (sec_entries_off, &sec_entries_buf),
        ],
    };

    let mut gpt_disk = gpt_disk_io::Disk::new(io)?;
    let mut scratch = vec![0u8; (bs as usize).max(16 * 1024)];
    let header = gpt_disk.read_primary_gpt_header(&mut scratch)?;
    let secondary_header = gpt_disk.read_secondary_gpt_header(&mut scratch)?;
    let part_array_layout = header.get_partition_entry_array_layout()?;
    let partitions = gpt_disk
        .gpt_partition_entry_array_iter(part_array_layout, &mut scratch)?
        .filter_map(|part| match part {
            Err(err) => Some(Err(Error::from(err))),
            Ok(part) => {
                if part.is_used() {
                    let s_lba = part.starting_lba.to_u64();
                    let e_lba = part.ending_lba.to_u64();
                    if e_lba < s_lba || e_lba >= num_blocks {
                        return Some(Err(Error::msg("Invalid GPT partition LBA bounds")));
                    }
                    let byte_start = s_lba.checked_mul(bs)?;
                    let byte_end = e_lba.checked_add(1)?.checked_mul(bs)?;
                    let guid = part.unique_partition_guid;
                    let name = part.name;
                    Some(Ok(DiskPartition {
                        byte_start,
                        byte_end,
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
        gpt_range(&header, bs)?,
        gpt_range(&secondary_header, bs)?,
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

enum DisksInitState {
    Uninit,
    Initializing(Vec<crate::executor::event::EventTrigger>),
    Ready,
}

static DISCOVERED_DISKS: spin::Mutex<Vec<DiskEntry>> = spin::Mutex::new(Vec::new());
static DISKS_INIT_STATE: spin::Mutex<DisksInitState> = spin::Mutex::new(DisksInitState::Uninit);

struct DevGuard {
    index: usize,
    dev: Option<KernelBlockDevice>,
}

impl DevGuard {
    fn acquire(index: usize) -> Result<Self> {
        let dev = DISCOVERED_DISKS
            .lock()
            .get_mut(index)
            .and_then(|e| e.dev.take())
            .ok_or_else(|| Error::msg("Disk device busy or unavailable"))?;
        Ok(Self {
            index,
            dev: Some(dev),
        })
    }

    fn get_mut(&mut self) -> &mut KernelBlockDevice {
        self.dev.as_mut().unwrap()
    }
}

impl Drop for DevGuard {
    fn drop(&mut self) {
        if let Some(dev) = self.dev.take()
            && let Some(entry) = DISCOVERED_DISKS.lock().get_mut(self.index)
        {
            entry.dev = Some(dev);
        }
    }
}

pub struct Disk {
    index: usize,
    size: u64,
    block_size: u32,
}

impl Disk {
    pub async fn init() {
        let waiter = {
            let mut st = DISKS_INIT_STATE.lock();
            match &mut *st {
                DisksInitState::Ready => return,
                DisksInitState::Initializing(waiters) => {
                    let ev = crate::executor::event::Event::new();
                    waiters.push(ev.trigger());
                    Some(ev)
                }
                DisksInitState::Uninit => {
                    *st = DisksInitState::Initializing(Vec::new());
                    None
                }
            }
        };
        if let Some(ev) = waiter {
            ev.await;
            return;
        }

        let hal = crate::hal::NativeHal;
        let pci_devs = pixie_drivers::pci::scan_pci(&hal);
        let mut entries = Vec::new();

        for &dev in &pci_devs {
            // NVMe controller: class 0x01, subclass 0x08
            if dev.class_code == 0x01 && dev.subclass == 0x08 {
                dev.enable_bus_mastering(&hal);
                if let Some(pixie_drivers::pci::Bar::Memory {
                    base: bar0, size, ..
                }) = dev.read_bar(&hal, 0)
                {
                    crate::arch::paging::map_mmio(bar0, size.max(0x10000));
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
                if let Some(pixie_drivers::pci::Bar::Memory {
                    base: abar, size, ..
                }) = dev.read_bar(&hal, 5)
                {
                    crate::arch::paging::map_mmio(abar, size.max(0x10000));
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
        let waiters = {
            let mut st = DISKS_INIT_STATE.lock();
            let old = core::mem::replace(&mut *st, DisksInitState::Ready);
            match old {
                DisksInitState::Initializing(w) => w,
                _ => Vec::new(),
            }
        };
        for w in waiters {
            w.trigger();
        }
    }

    pub async fn largest() -> Self {
        Self::init().await;
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
        Self::init().await;
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
        let mut guard = DevGuard::acquire(self.index)?;
        guard.get_mut().read(offset, buf).await
    }

    pub async fn write(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        let mut guard = DevGuard::acquire(self.index)?;
        guard.get_mut().write(offset, buf).await
    }

    pub async fn flush(&mut self) -> Result<()> {
        let mut guard = DevGuard::acquire(self.index)?;
        guard.get_mut().flush().await
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
