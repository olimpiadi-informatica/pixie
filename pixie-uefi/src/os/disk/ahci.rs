use alloc::vec::Vec;
use core::ptr::{self, NonNull};
use core::sync::atomic::{Ordering, fence};
use core::time::Duration;

use crate::os::error::{Error, Result};
use crate::os::executor::Executor;
use crate::os::memory::{self, PAGE_SIZE};
use crate::os::pci::{PciDevice, scan_pci};
use crate::os::timer::Timer;

const ATA_CMD_READ_DMA_EXT: u8 = 0x25;
const ATA_CMD_WRITE_DMA_EXT: u8 = 0x35;
const ATA_CMD_IDENTIFY: u8 = 0xEC;
const ATA_CMD_FLUSH_EXT: u8 = 0xEA;

const PORT_REG_CLB: usize = 0x00;
const PORT_REG_CLBU: usize = 0x04;
const PORT_REG_FB: usize = 0x08;
const PORT_REG_FBU: usize = 0x0C;
const PORT_REG_IS: usize = 0x10;
const PORT_REG_CMD: usize = 0x18;
const PORT_REG_TFD: usize = 0x20;
const PORT_REG_SIG: usize = 0x24;
const PORT_REG_SSTS: usize = 0x28;
const PORT_REG_SERR: usize = 0x30;
const PORT_REG_CI: usize = 0x38;

const COMMAND_TIMEOUT_MICROS: i64 = 10_000_000;
const AHCI_DMA_PAGES: usize = 32; // 128 KiB transfer buffer

struct DmaBuffer {
    phys: u64,
    ptr: NonNull<u8>,
    pages: usize,
}

impl DmaBuffer {
    fn new(pages: usize) -> Result<Self> {
        let phys = memory::alloc_contiguous(pages)
            .ok_or_else(|| Error::msg("Out of physical memory for AHCI DMA"))?;
        let ptr = NonNull::new(phys as *mut u8).unwrap();
        unsafe { ptr::write_bytes(ptr.as_ptr(), 0, pages * PAGE_SIZE as usize) };
        Ok(Self { phys, ptr, pages })
    }

    fn as_ptr<T>(&self) -> *mut T {
        self.ptr.as_ptr().cast()
    }

    fn as_bytes(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.pages * PAGE_SIZE as usize) }
    }

    fn as_bytes_mut(&mut self) -> &mut [u8] {
        unsafe {
            core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.pages * PAGE_SIZE as usize)
        }
    }

    fn phys(&self) -> u64 {
        self.phys
    }
}

impl Drop for DmaBuffer {
    fn drop(&mut self) {
        memory::free_contiguous(self.phys, self.pages);
    }
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
struct AhciCmdHeader {
    flags: u16,
    prdtl: u16,
    prdbc: u32,
    ctba: u32,
    ctbau: u32,
    reserved: [u32; 4],
}

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
struct AhciPrdtEntry {
    dba: u32,
    dbau: u32,
    reserved: u32,
    dbc: u32, // Byte count (0-indexed) | bit 31 = Interrupt on Completion
}

#[allow(dead_code)]
pub struct AhciDisk {
    pub pci: PciDevice,
    abar: u64,
    port_idx: usize,
    port_base: u64,
    _cmd_list: DmaBuffer,
    _recv_fis: DmaBuffer,
    cmd_table: DmaBuffer,
    data: DmaBuffer,
    block_size: u64,
    blocks: u64,
}

impl AhciDisk {
    fn read32(base: u64, offset: usize) -> u32 {
        unsafe { core::ptr::read_volatile((base + offset as u64) as *const u32) }
    }

    fn write32(base: u64, offset: usize, val: u32) {
        unsafe { core::ptr::write_volatile((base + offset as u64) as *mut u32, val) }
    }

    fn port_read32(&self, offset: usize) -> u32 {
        Self::read32(self.port_base, offset)
    }

    fn port_write32(&self, offset: usize, val: u32) {
        Self::write32(self.port_base, offset, val)
    }

    pub async fn open(pci: PciDevice, port_idx: usize) -> Result<Self> {
        pci.enable_bus_mastering();
        let abar = match pci.read_bar(5) {
            Some(crate::os::pci::Bar::Memory { base, .. }) => base,
            _ => return Err(Error::msg("AHCI ABAR (BAR5) is not memory-mapped")),
        };

        // Enable AHCI mode in Generic Host Control (offset 0x04)
        let ghc = Self::read32(abar, 0x04);
        Self::write32(abar, 0x04, ghc | (1 << 31));

        let port_base = abar + 0x100 + (port_idx as u64 * 0x80);

        let cmd_list = DmaBuffer::new(1)?; // 4KB (Command list needs 1KB)
        let recv_fis = DmaBuffer::new(1)?; // 4KB (FIS needs 256 bytes)
        let cmd_table = DmaBuffer::new(1)?; // 4KB (Command table + PRDT)
        let data = DmaBuffer::new(AHCI_DMA_PAGES)?; // 128KB data buffer

        let mut disk = Self {
            pci,
            abar,
            port_idx,
            port_base,
            _cmd_list: cmd_list,
            _recv_fis: recv_fis,
            cmd_table,
            data,
            block_size: 512,
            blocks: 0,
        };

        disk.init_port()?;
        disk.identify().await?;
        Ok(disk)
    }

    fn stop_port(&self) -> Result<()> {
        // 1. Clear ST
        let cmd = self.port_read32(PORT_REG_CMD);
        self.port_write32(PORT_REG_CMD, cmd & !1);

        // 2. Wait for CR (bit 15) to clear
        let deadline = Timer::micros() + 500_000;
        while (self.port_read32(PORT_REG_CMD) & (1 << 15)) != 0 {
            if Timer::micros() > deadline {
                return Err(Error::msg("AHCI port stop timeout waiting for CR=0"));
            }
            crate::os::arch::io::pause();
        }

        // 3. Clear FRE (bit 4)
        let cmd = self.port_read32(PORT_REG_CMD);
        self.port_write32(PORT_REG_CMD, cmd & !(1 << 4));

        // 4. Wait for FR (bit 14) to clear
        let deadline = Timer::micros() + 500_000;
        while (self.port_read32(PORT_REG_CMD) & (1 << 14)) != 0 {
            if Timer::micros() > deadline {
                return Err(Error::msg("AHCI port stop timeout waiting for FR=0"));
            }
            crate::os::arch::io::pause();
        }

        Ok(())
    }

    fn start_port(&self) {
        let deadline = Timer::micros() + 500_000;
        while (self.port_read32(PORT_REG_CMD) & (1 << 15)) != 0 {
            if Timer::micros() > deadline {
                break;
            }
            crate::os::arch::io::pause();
        }
        let cmd = self.port_read32(PORT_REG_CMD);
        self.port_write32(PORT_REG_CMD, cmd | (1 << 4)); // FRE
        self.port_write32(PORT_REG_CMD, cmd | (1 << 4) | 1); // FRE | ST
    }

    fn init_port(&mut self) -> Result<()> {
        self.stop_port()?;

        // Configure Command List Base and FIS Base
        self.port_write32(PORT_REG_CLB, self._cmd_list.phys() as u32);
        self.port_write32(PORT_REG_CLBU, (self._cmd_list.phys() >> 32) as u32);
        self.port_write32(PORT_REG_FB, self._recv_fis.phys() as u32);
        self.port_write32(PORT_REG_FBU, (self._recv_fis.phys() >> 32) as u32);

        // Clear error and interrupt status
        self.port_write32(PORT_REG_SERR, 0xFFFF_FFFF);
        self.port_write32(PORT_REG_IS, 0xFFFF_FFFF);

        self.start_port();
        Ok(())
    }

    async fn issue_cmd(
        &mut self,
        cmd: u8,
        write: bool,
        lba: u64,
        count: u16,
        buf_phys: u64,
        byte_count: usize,
    ) -> Result<()> {
        // Setup Command Header 0
        let hdr = AhciCmdHeader {
            flags: 5 | (if write { 1 << 6 } else { 0 }), // 5 dwords FIS
            prdtl: 1,
            prdbc: 0,
            ctba: self.cmd_table.phys() as u32,
            ctbau: (self.cmd_table.phys() >> 32) as u32,
            reserved: [0; 4],
        };
        unsafe {
            ptr::write_volatile(self._cmd_list.as_ptr::<AhciCmdHeader>(), hdr);
        }

        // Setup Command Table
        let cmd_tbl_ptr = self.cmd_table.as_ptr::<u8>();
        unsafe {
            ptr::write_bytes(cmd_tbl_ptr, 0, 128); // Clear Command FIS
        }

        // Setup Command FIS (Register H2D)
        let fis = unsafe { core::slice::from_raw_parts_mut(cmd_tbl_ptr, 20) };
        fis[0] = 0x27; // FIS Type: Register FIS H2D
        fis[1] = 0x80; // C = 1 (Command)
        fis[2] = cmd; // ATA command opcode
        fis[4] = (lba & 0xFF) as u8;
        fis[5] = ((lba >> 8) & 0xFF) as u8;
        fis[6] = ((lba >> 16) & 0xFF) as u8;
        fis[7] = 1 << 6; // LBA mode
        fis[8] = ((lba >> 24) & 0xFF) as u8;
        fis[9] = ((lba >> 32) & 0xFF) as u8;
        fis[10] = ((lba >> 40) & 0xFF) as u8;
        fis[12] = (count & 0xFF) as u8;
        fis[13] = ((count >> 8) & 0xFF) as u8;

        // Setup PRDT Entry 0 (located at offset 0x80 in Command Table)
        let prdt = AhciPrdtEntry {
            dba: buf_phys as u32,
            dbau: (buf_phys >> 32) as u32,
            reserved: 0,
            dbc: ((byte_count - 1) as u32) | (1 << 31), // Bit 31: Interrupt on Completion
        };
        unsafe {
            let prdt_ptr = cmd_tbl_ptr.add(0x80).cast::<AhciPrdtEntry>();
            ptr::write_volatile(prdt_ptr, prdt);
        }

        fence(Ordering::Release);

        // Issue command on slot 0
        self.port_write32(PORT_REG_CI, 1);

        // Fast path: brief spin for ultra-fast completions (< 50 us)
        for _ in 0..100 {
            if (self.port_read32(PORT_REG_CI) & 1) == 0 {
                let tfd = self.port_read32(PORT_REG_TFD);
                if (tfd & 1) != 0 {
                    return Err(Error::msg("AHCI command error (TFD ERR)"));
                }
                return Ok(());
            }
            core::hint::spin_loop();
        }

        // Async path: cooperative sleep allowing executor to run other tasks or halt CPU
        let deadline = Timer::micros() + COMMAND_TIMEOUT_MICROS;
        loop {
            if (self.port_read32(PORT_REG_CI) & 1) == 0 {
                // Check errors
                let tfd = self.port_read32(PORT_REG_TFD);
                if (tfd & 1) != 0 {
                    return Err(Error::msg("AHCI command error (TFD ERR)"));
                }
                return Ok(());
            }
            if Timer::micros() > deadline {
                return Err(Error::msg("AHCI command timeout"));
            }
            Executor::sleep(Duration::from_micros(100)).await;
        }
    }

    fn issue_cmd_sync(
        &mut self,
        cmd: u8,
        write: bool,
        lba: u64,
        count: u16,
        buf_phys: u64,
        byte_count: usize,
    ) -> Result<()> {
        let hdr = AhciCmdHeader {
            flags: 5 | (if write { 1 << 6 } else { 0 }),
            prdtl: 1,
            prdbc: 0,
            ctba: self.cmd_table.phys() as u32,
            ctbau: (self.cmd_table.phys() >> 32) as u32,
            reserved: [0; 4],
        };
        unsafe {
            ptr::write_volatile(self._cmd_list.as_ptr::<AhciCmdHeader>(), hdr);
        }

        let cmd_tbl_ptr = self.cmd_table.as_ptr::<u8>();
        unsafe {
            ptr::write_bytes(cmd_tbl_ptr, 0, 128);
        }

        let fis = unsafe { core::slice::from_raw_parts_mut(cmd_tbl_ptr, 20) };
        fis[0] = 0x27;
        fis[1] = 0x80;
        fis[2] = cmd;
        fis[4] = (lba & 0xFF) as u8;
        fis[5] = ((lba >> 8) & 0xFF) as u8;
        fis[6] = ((lba >> 16) & 0xFF) as u8;
        fis[7] = 1 << 6;
        fis[8] = ((lba >> 24) & 0xFF) as u8;
        fis[9] = ((lba >> 32) & 0xFF) as u8;
        fis[10] = ((lba >> 40) & 0xFF) as u8;
        fis[12] = (count & 0xFF) as u8;
        fis[13] = ((count >> 8) & 0xFF) as u8;

        let prdt = AhciPrdtEntry {
            dba: buf_phys as u32,
            dbau: (buf_phys >> 32) as u32,
            reserved: 0,
            dbc: ((byte_count - 1) as u32) | (1 << 31),
        };
        unsafe {
            let prdt_ptr = cmd_tbl_ptr.add(0x80).cast::<AhciPrdtEntry>();
            ptr::write_volatile(prdt_ptr, prdt);
        }

        fence(Ordering::Release);

        self.port_write32(PORT_REG_CI, 1);

        let deadline = Timer::micros() + 10_000_000; // 10 seconds
        loop {
            if (self.port_read32(PORT_REG_CI) & 1) == 0 {
                let tfd = self.port_read32(PORT_REG_TFD);
                if (tfd & 1) != 0 {
                    return Err(Error::msg("AHCI command error (TFD ERR sync)"));
                }
                return Ok(());
            }
            if Timer::micros() > deadline {
                return Err(Error::msg("AHCI command timeout (sync)"));
            }
            crate::os::arch::io::pause();
        }
    }

    async fn identify(&mut self) -> Result<()> {
        let phys = self.data.phys();
        self.issue_cmd(ATA_CMD_IDENTIFY, false, 0, 0, phys, 512)
            .await?;

        let bytes = self.data.as_bytes();
        let sectors_48 = u64::from_le_bytes(bytes[200..208].try_into().unwrap());
        let sectors_28 = u32::from_le_bytes(bytes[120..124].try_into().unwrap()) as u64;

        self.blocks = if sectors_48 > 0 {
            sectors_48
        } else {
            sectors_28
        };
        self.block_size = 512;

        if self.blocks == 0 {
            return Err(Error::msg("AHCI SATA drive reported 0 blocks"));
        }

        Ok(())
    }

    pub fn size(&self) -> u64 {
        self.block_size.saturating_mul(self.blocks)
    }

    pub fn block_size(&self) -> u32 {
        self.block_size as u32
    }

    pub fn num_blocks(&self) -> u64 {
        self.blocks
    }

    pub async fn flush(&mut self) -> Result<()> {
        let phys = self.data.phys();
        self.issue_cmd(ATA_CMD_FLUSH_EXT, false, 0, 0, phys, 512)
            .await
    }

    pub fn read_sync(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let mut cur_offset = offset;
        let mut remaining = buf;
        let max_blocks = (self.data.pages * PAGE_SIZE as usize) / self.block_size as usize;
        while !remaining.is_empty() {
            let lba = cur_offset / self.block_size;
            let in_block = (cur_offset % self.block_size) as usize;
            let blocks = ((in_block + remaining.len()).div_ceil(self.block_size as usize))
                .min(max_blocks);

            let phys = self.data.phys();
            self.issue_cmd_sync(
                ATA_CMD_READ_DMA_EXT,
                false,
                lba,
                blocks as u16,
                phys,
                blocks * self.block_size as usize,
            )?;
            let chunk_bytes = (blocks * self.block_size as usize - in_block).min(remaining.len());
            remaining[..chunk_bytes]
                .copy_from_slice(&self.data.as_bytes()[in_block..in_block + chunk_bytes]);

            cur_offset += chunk_bytes as u64;
            remaining = &mut remaining[chunk_bytes..];
        }
        Ok(())
    }

    pub async fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let mut cur_offset = offset;
        let mut remaining = buf;
        let max_blocks = (self.data.pages * PAGE_SIZE as usize) / self.block_size as usize;
        while !remaining.is_empty() {
            let lba = cur_offset / self.block_size;
            let in_block = (cur_offset % self.block_size) as usize;
            let blocks = ((in_block + remaining.len()).div_ceil(self.block_size as usize))
                .min(max_blocks);

            let phys = self.data.phys();
            self.issue_cmd(
                ATA_CMD_READ_DMA_EXT,
                false,
                lba,
                blocks as u16,
                phys,
                blocks * self.block_size as usize,
            )
            .await?;
            let chunk_bytes = (blocks * self.block_size as usize - in_block).min(remaining.len());
            remaining[..chunk_bytes]
                .copy_from_slice(&self.data.as_bytes()[in_block..in_block + chunk_bytes]);

            cur_offset += chunk_bytes as u64;
            remaining = &mut remaining[chunk_bytes..];
        }
        Ok(())
    }

    pub fn write_sync(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        let mut cur_offset = offset;
        let mut remaining = buf;
        let max_blocks = (self.data.pages * PAGE_SIZE as usize) / self.block_size as usize;
        while !remaining.is_empty() {
            let lba = cur_offset / self.block_size;
            let in_block = (cur_offset % self.block_size) as usize;
            let blocks = ((in_block + remaining.len()).div_ceil(self.block_size as usize))
                .min(max_blocks);

            let phys = self.data.phys();
            if in_block != 0 || remaining.len() < blocks * self.block_size as usize {
                self.issue_cmd_sync(
                    ATA_CMD_READ_DMA_EXT,
                    false,
                    lba,
                    blocks as u16,
                    phys,
                    blocks * self.block_size as usize,
                )?;
            }

            let chunk_bytes = (blocks * self.block_size as usize - in_block).min(remaining.len());
            self.data.as_bytes_mut()[in_block..in_block + chunk_bytes]
                .copy_from_slice(&remaining[..chunk_bytes]);

            self.issue_cmd_sync(
                ATA_CMD_WRITE_DMA_EXT,
                true,
                lba,
                blocks as u16,
                phys,
                blocks * self.block_size as usize,
            )?;

            cur_offset += chunk_bytes as u64;
            remaining = &remaining[chunk_bytes..];
        }
        Ok(())
    }

    pub async fn write(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        let mut cur_offset = offset;
        let mut remaining = buf;
        let max_blocks = (self.data.pages * PAGE_SIZE as usize) / self.block_size as usize;
        while !remaining.is_empty() {
            let lba = cur_offset / self.block_size;
            let in_block = (cur_offset % self.block_size) as usize;
            let blocks = ((in_block + remaining.len()).div_ceil(self.block_size as usize))
                .min(max_blocks);

            let phys = self.data.phys();
            if in_block != 0 || remaining.len() < blocks * self.block_size as usize {
                self.issue_cmd(
                    ATA_CMD_READ_DMA_EXT,
                    false,
                    lba,
                    blocks as u16,
                    phys,
                    blocks * self.block_size as usize,
                )
                .await?;
            }

            let chunk_bytes = (blocks * self.block_size as usize - in_block).min(remaining.len());
            self.data.as_bytes_mut()[in_block..in_block + chunk_bytes]
                .copy_from_slice(&remaining[..chunk_bytes]);

            self.issue_cmd(
                ATA_CMD_WRITE_DMA_EXT,
                true,
                lba,
                blocks as u16,
                phys,
                blocks * self.block_size as usize,
            )
            .await?;

            cur_offset += chunk_bytes as u64;
            remaining = &remaining[chunk_bytes..];
        }
        Ok(())
    }

    pub async fn probe_all() -> Vec<AhciDisk> {
        let mut disks = Vec::new();
        for dev in scan_pci() {
            if dev.class_code == 0x01 && dev.subclass == 0x06 && dev.prog_if == 0x01 {
                dev.enable_bus_mastering();
                log::info!(
                    "Probing AHCI SATA controller at {:02x}:{:02x}.{}",
                    dev.bus,
                    dev.dev,
                    dev.func
                );
                let abar = match dev.read_bar(5) {
                    Some(crate::os::pci::Bar::Memory { base, .. }) => base,
                    _ => {
                        log::warn!("AHCI ABAR (BAR5) is not memory-mapped");
                        continue;
                    }
                };

                // Enable AHCI mode in Generic Host Control (offset 0x04)
                let ghc = Self::read32(abar, 0x04);
                Self::write32(abar, 0x04, ghc | (1 << 31));

                let pi = Self::read32(abar, 0x0C);
                log::debug!("AHCI abar: 0x{abar:X}, pi: 0x{pi:X}");
                for port in 0..32 {
                    if (pi & (1 << port)) == 0 {
                        continue;
                    }
                    let port_base = abar + 0x100 + (port as u64 * 0x80);
                    let ssts = Self::read32(port_base, PORT_REG_SSTS);
                    let det = ssts & 0x0F;
                    let ipm = (ssts >> 8) & 0x0F;
                    let sig = Self::read32(port_base, PORT_REG_SIG);
                    log::debug!(
                        "AHCI port {port}: ssts=0x{ssts:X} (det={det}, ipm={ipm}), sig=0x{sig:X}"
                    );
                    if det == 3 && ipm == 1 && (sig == 0x0000_0101 || sig == 0) {
                        log::info!("Found SATA ATA drive on port {port}");
                        match Self::open(dev, port).await {
                            Ok(disk) => {
                                log::info!(
                                    "SATA drive initialized: {} blocks of {} bytes ({} GB)",
                                    disk.blocks,
                                    disk.block_size,
                                    disk.size() / (1024 * 1024 * 1024)
                                );
                                disks.push(disk);
                            }
                            Err(e) => {
                                log::warn!("Failed to initialize SATA port {port}: {e}");
                            }
                        }
                    }
                }
            }
        }
        disks
    }
}
