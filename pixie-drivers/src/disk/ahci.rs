use alloc::format;
use alloc::vec::Vec;
use core::ptr;
use core::sync::atomic::{Ordering, fence};
use core::time::Duration;

use crate::disk::{BlockDeviceIo, read_bytes, write_bytes};
use crate::error::{Error, Result};
use crate::hal::{DmaBuffer, Hal, PAGE_SIZE, alloc_dma_buffers};
use crate::pci::PciDevice;

pub const AHCI_MAX_DMA_PAGES: usize = 64; // 256 KiB per command via scatter-gather PRDT

// Single 4-KiB control DMA page layout:
// +0x000..0x400: Command List (32 headers * 32 B = 1024 B, 1-KiB aligned)
// +0x400..0x500: Received FIS buffer (256 B, 256-B aligned)
// +0x500..0x980: Command Table 0 (128 B CFIS/ACMD/Rsvd + 64 * 16 B PRDT = 1152 B, 128-B aligned)
const OFF_CMD_LIST: usize = 0x000;
const OFF_RECV_FIS: usize = 0x400;
const OFF_CMD_TABLE: usize = 0x500;

const ATA_CMD_READ_DMA: u8 = 0xC8;
const ATA_CMD_WRITE_DMA: u8 = 0xCA;
const ATA_CMD_FLUSH: u8 = 0xE7;
const ATA_CMD_READ_DMA_EXT: u8 = 0x25;
const ATA_CMD_WRITE_DMA_EXT: u8 = 0x35;
const ATA_CMD_IDENTIFY: u8 = 0xEC;
const ATA_CMD_FLUSH_EXT: u8 = 0xEA;

const PORT_REG_CLB: u64 = 0x00;
const PORT_REG_CLBU: u64 = 0x04;
const PORT_REG_FB: u64 = 0x08;
const PORT_REG_FBU: u64 = 0x0C;
const PORT_REG_IS: u64 = 0x10;
const PORT_REG_CMD: u64 = 0x18;
const PORT_REG_TFD: u64 = 0x20;
const PORT_REG_SIG: u64 = 0x24;
const PORT_REG_SSTS: u64 = 0x28;
const PORT_REG_SCTL: u64 = 0x2C;
const PORT_REG_SERR: u64 = 0x30;
const PORT_REG_CI: u64 = 0x38;

const PORT_IS_TFES: u32 = 1 << 30;
const PORT_IS_HBFS: u32 = 1 << 29;
const PORT_IS_HBDS: u32 = 1 << 28;
const PORT_IS_IFS: u32 = 1 << 27;
const PORT_IS_OFS: u32 = 1 << 24;
const PORT_IS_ERRORS: u32 = PORT_IS_TFES | PORT_IS_HBFS | PORT_IS_HBDS | PORT_IS_IFS | PORT_IS_OFS;

const COMMAND_TIMEOUT_MICROS: i64 = 10_000_000;
const MAX_POLL_YIELDS: usize = 10_000_000;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AhciCmdHeader {
    flags: u16,
    prdtl: u16,
    prdbc: u32,
    ctba: u32,
    ctbau: u32,
    reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AhciPrdtEntry {
    dba: u32,
    dbau: u32,
    reserved: u32,
    dbc: u32,
}

const _: () = assert!(core::mem::size_of::<AhciCmdHeader>() == 32);
const _: () = assert!(core::mem::size_of::<AhciPrdtEntry>() == 16);

pub struct AhciDisk<H: Hal> {
    pub pci: PciDevice,
    pub abar: u64,
    pub port_idx: usize,
    pub hal: H,
    port_base: u64,
    ctrl: DmaBuffer,
    data: DmaBuffer,
    block_size: u32,
    blocks: u64,
    lba48: bool,
}

impl<H: Hal> AhciDisk<H> {
    fn bios_handoff_sync(abar: u64, hal: &H) {
        let cap2 = hal.mmio_read32(abar + 0x24);
        if (cap2 & 1) != 0 {
            let bohc = hal.mmio_read32(abar + 0x28);
            hal.mmio_write32(abar + 0x28, bohc | (1 << 1));
            let start = hal.micros();
            for _ in 0..10_000 {
                if (hal.mmio_read32(abar + 0x28) & 1) == 0 {
                    break;
                }
                if hal.micros().saturating_sub(start) > 25_000 {
                    break;
                }
            }
        }
    }

    /// Performs BIOS/OS handoff on `abar`, enables `GHC.AE`, and returns the port indices (`0..32`)
    /// that have a present SATA disk (`PxSSTS.DET == 3` and signature `0x00000101` or ready for probe).
    pub fn probe_sata_ports(abar: u64, hal: &H) -> Vec<usize> {
        let mut ports = Vec::new();
        if abar == 0 {
            return ports;
        }
        hal.map_mmio(abar, 0x1100);
        Self::bios_handoff_sync(abar, hal);

        let ghc = hal.mmio_read32(abar + 0x04);
        hal.mmio_write32(abar + 0x04, ghc | (1 << 31));

        let pi = hal.mmio_read32(abar + 0x0C);
        for port in 0..32 {
            if (pi & (1 << port)) == 0 {
                continue;
            }
            let port_base = abar + 0x100 + (port as u64) * 0x80;
            let ssts = hal.mmio_read32(port_base + PORT_REG_SSTS);
            let det = ssts & 0x0F;
            let sig = hal.mmio_read32(port_base + PORT_REG_SIG);
            if det == 3 && (sig == 0x0000_0101 || sig == 0 || sig == 0xFFFF_FFFF) {
                ports.push(port);
            }
        }
        ports
    }

    #[inline]
    fn cmd_list_phys(&self) -> u64 {
        self.ctrl.phys + OFF_CMD_LIST as u64
    }

    #[inline]
    fn recv_fis_phys(&self) -> u64 {
        self.ctrl.phys + OFF_RECV_FIS as u64
    }

    #[inline]
    fn cmd_table_phys(&self) -> u64 {
        self.ctrl.phys + OFF_CMD_TABLE as u64
    }

    fn port_read32(&self, offset: u64) -> u32 {
        self.hal.mmio_read32(self.port_base + offset)
    }

    fn port_write32(&self, offset: u64, val: u32) {
        self.hal.mmio_write32(self.port_base + offset, val);
    }

    fn program_port_dma(&self) {
        let clb = self.cmd_list_phys();
        let fb = self.recv_fis_phys();
        self.port_write32(PORT_REG_CLB, clb as u32);
        self.port_write32(PORT_REG_CLBU, (clb >> 32) as u32);
        self.port_write32(PORT_REG_FB, fb as u32);
        self.port_write32(PORT_REG_FBU, (fb >> 32) as u32);
    }

    /// Initializes port `port_idx` on the AHCI controller at `abar`.
    /// - Performs BIOS/OS handoff (`BOHC`) and sets `GHC.AE`.
    /// - If `PxSSTS.DET == 3` and `PxTFD` has `BSY == 0 && DRQ == 0`, performs a non-destructive
    ///   port stop/reconfigure/start without issuing a link-disrupting `COMRESET`.
    /// - If the port is busy/stuck (`BSY != 0 || DRQ != 0`), performs `COMRESET` with `PxCMD.FRE = 1`
    ///   enabled before waiting for `PxTFD.BSY == 0` so the initial D2H Register FIS is received.
    /// - Identifies the ATA device (`0xEC`).
    pub async fn open(pci: PciDevice, abar: u64, port_idx: usize, hal: H) -> Result<Self> {
        if abar == 0 || port_idx >= 32 {
            return Err(Error::msg("Invalid AHCI ABAR or port index"));
        }
        pci.enable_bus_mastering(&hal);
        hal.map_mmio(abar, 0x1100);

        Self::bios_handoff_sync(abar, &hal);
        let ghc = hal.mmio_read32(abar + 0x04);
        hal.mmio_write32(abar + 0x04, ghc | (1 << 31));

        let port_base = abar + 0x100 + (port_idx as u64) * 0x80;
        let [ctrl, data] =
            alloc_dma_buffers(&hal, [1, AHCI_MAX_DMA_PAGES], "Out of DMA memory for AHCI")?;

        let mut disk = Self {
            pci,
            abar,
            port_idx,
            hal,
            port_base,
            ctrl,
            data,
            block_size: 512,
            blocks: 0,
            lba48: false,
        };

        disk.init_port().await?;
        disk.identify().await?;
        Ok(disk)
    }

    async fn wait_port_cond<F: Fn(&Self) -> bool>(&self, timeout_us: i64, cond: F) -> bool {
        let start = self.hal.micros();
        for _ in 0..MAX_POLL_YIELDS {
            if cond(self) {
                return true;
            }
            if self.hal.micros().saturating_sub(start) > timeout_us {
                return cond(self);
            }
            self.hal.yield_now().await;
        }
        cond(self)
    }

    async fn stop_port(&self) -> Result<()> {
        self.port_write32(PORT_REG_SERR, 0xFFFF_FFFF);
        self.port_write32(PORT_REG_IS, 0xFFFF_FFFF);

        let cmd = self.port_read32(PORT_REG_CMD) & 0x0FFF_FFFF;
        self.port_write32(PORT_REG_CMD, cmd & !1);
        if !self
            .wait_port_cond(500_000, |s| (s.port_read32(PORT_REG_CMD) & (1 << 15)) == 0)
            .await
        {
            return Err(Error::msg("AHCI port stop timeout waiting for CR=0"));
        }

        let cmd = self.port_read32(PORT_REG_CMD) & 0x0FFF_FFFF;
        self.port_write32(PORT_REG_CMD, cmd & !(1 << 4));
        if !self
            .wait_port_cond(500_000, |s| (s.port_read32(PORT_REG_CMD) & (1 << 14)) == 0)
            .await
        {
            return Err(Error::msg("AHCI port stop timeout waiting for FR=0"));
        }

        Ok(())
    }

    async fn comreset_with_fre(&self) -> Result<()> {
        // Ensure PxCMD.FRE (bit 4) and SUD (bit 1) are enabled BEFORE COMRESET
        // so the port accepts the initial D2H Register FIS into the FIS receive buffer.
        let cmd = self.port_read32(PORT_REG_CMD) & 0x0FFF_FFFF;
        self.port_write32(PORT_REG_CMD, (cmd & !1) | (1 << 1) | (1 << 4));

        let sctl = self.port_read32(PORT_REG_SCTL);
        self.port_write32(PORT_REG_SCTL, (sctl & !0x0F) | 0x01);
        self.hal.sleep(Duration::from_millis(2)).await;
        let sctl = self.port_read32(PORT_REG_SCTL);
        self.port_write32(PORT_REG_SCTL, sctl & !0x0F);

        if !self
            .wait_port_cond(500_000, |s| (s.port_read32(PORT_REG_SSTS) & 0x0F) == 3)
            .await
        {
            return Err(Error::msg("AHCI COMRESET timeout: device not detected"));
        }

        self.port_write32(PORT_REG_SERR, 0xFFFF_FFFF);
        self.port_write32(PORT_REG_IS, 0xFFFF_FFFF);

        // Wait for BSY (bit 7) and DRQ (bit 3) to clear
        if !self
            .wait_port_cond(2_000_000, |s| (s.port_read32(PORT_REG_TFD) & 0x88) == 0)
            .await
        {
            return Err(Error::msg("AHCI drive still busy after COMRESET"));
        }

        self.port_write32(PORT_REG_SERR, 0xFFFF_FFFF);
        self.port_write32(PORT_REG_IS, 0xFFFF_FFFF);
        Ok(())
    }

    async fn start_port(&self) {
        let _ = self
            .wait_port_cond(500_000, |s| (s.port_read32(PORT_REG_CMD) & (1 << 15)) == 0)
            .await;

        let cmd = self.port_read32(PORT_REG_CMD) & 0x0FFF_FFFF;
        self.port_write32(PORT_REG_CMD, cmd | (1 << 1) | (1 << 4));

        self.port_write32(PORT_REG_SERR, 0xFFFF_FFFF);
        self.port_write32(PORT_REG_IS, 0xFFFF_FFFF);

        let cmd = self.port_read32(PORT_REG_CMD) & 0x0FFF_FFFF;
        self.port_write32(PORT_REG_CMD, cmd | (1 << 1) | (1 << 4) | 1);
    }

    async fn init_port(&mut self) -> Result<()> {
        self.stop_port().await?;
        self.program_port_dma();

        // Enable FRE (bit 4) and SUD (bit 1) BEFORE checking link or issuing COMRESET
        let cmd = self.port_read32(PORT_REG_CMD) & 0x0FFF_FFFF;
        self.port_write32(PORT_REG_CMD, (cmd & !1) | (1 << 1) | (1 << 4));

        let ssts = self.port_read32(PORT_REG_SSTS);
        let tfd = self.port_read32(PORT_REG_TFD);
        if (ssts & 0x0F) != 3 || (tfd & 0x88) != 0 {
            self.comreset_with_fre().await?;
        } else {
            self.port_write32(PORT_REG_SERR, 0xFFFF_FFFF);
            self.port_write32(PORT_REG_IS, 0xFFFF_FFFF);
        }

        let sig = self.port_read32(PORT_REG_SIG);
        if sig != 0x0000_0101 && sig != 0 && sig != 0xFFFF_FFFF {
            return Err(Error::msg("Unsupported AHCI device signature"));
        }

        self.start_port().await;
        Ok(())
    }

    async fn recover_port(&self) -> Result<()> {
        let _ = self.stop_port().await;
        self.program_port_dma();
        self.comreset_with_fre().await?;
        self.start_port().await;
        Ok(())
    }

    async fn issue_cmd_once(
        &mut self,
        cmd: u8,
        write: bool,
        lba: u64,
        count: u32,
        byte_count: usize,
    ) -> Result<()> {
        if !self
            .wait_port_cond(1_000_000, |s| (s.port_read32(PORT_REG_TFD) & 0x88) == 0)
            .await
        {
            let _ = self.recover_port().await;
        }
        if (self.port_read32(PORT_REG_TFD) & 0x88) != 0 {
            return Err(Error::msg("AHCI port stuck busy before command"));
        }

        self.port_write32(PORT_REG_SERR, 0xFFFF_FFFF);
        self.port_write32(PORT_REG_IS, 0xFFFF_FFFF);

        let num_prdt = if byte_count == 0 {
            0
        } else {
            byte_count.div_ceil(PAGE_SIZE)
        };

        let ctba = self.cmd_table_phys();
        let hdr = AhciCmdHeader {
            flags: 5 | (if write { 1 << 6 } else { 0 }),
            prdtl: num_prdt as u16,
            prdbc: 0,
            ctba: ctba as u32,
            ctbau: (ctba >> 32) as u32,
            reserved: [0; 4],
        };
        unsafe {
            ptr::write_volatile(
                self.ctrl.virt.add(OFF_CMD_LIST).cast::<AhciCmdHeader>(),
                hdr,
            );
        }

        let cmd_tbl_ptr = unsafe { self.ctrl.virt.add(OFF_CMD_TABLE) };
        unsafe {
            ptr::write_bytes(cmd_tbl_ptr, 0, 128);
        }

        let fis = unsafe { core::slice::from_raw_parts_mut(cmd_tbl_ptr, 20) };
        fis[0] = 0x27; // Register H2D FIS
        fis[1] = 0x80; // Command bit
        fis[2] = cmd;
        if cmd == ATA_CMD_IDENTIFY {
            fis[7] = 0;
        } else if self.lba48 {
            let raw_cnt = if count == 65536 { 0u16 } else { count as u16 };
            fis[4] = (lba & 0xFF) as u8;
            fis[5] = ((lba >> 8) & 0xFF) as u8;
            fis[6] = ((lba >> 16) & 0xFF) as u8;
            fis[7] = 1 << 6;
            fis[8] = ((lba >> 24) & 0xFF) as u8;
            fis[9] = ((lba >> 32) & 0xFF) as u8;
            fis[10] = ((lba >> 40) & 0xFF) as u8;
            fis[12] = (raw_cnt & 0xFF) as u8;
            fis[13] = (raw_cnt >> 8) as u8;
        } else {
            // In ATA LBA28, sector count 256 is encoded as 0x00 in the 8-bit count field.
            fis[4] = (lba & 0xFF) as u8;
            fis[5] = ((lba >> 8) & 0xFF) as u8;
            fis[6] = ((lba >> 16) & 0xFF) as u8;
            fis[7] = (0xE0 | ((lba >> 24) & 0x0F)) as u8;
            fis[12] = if count == 256 {
                0
            } else {
                (count & 0xFF) as u8
            };
        }

        for i in 0..num_prdt {
            let page_phys = self.data.phys + (i * PAGE_SIZE) as u64;
            let chunk_len = (byte_count - i * PAGE_SIZE).min(PAGE_SIZE);
            let ioc = if i + 1 == num_prdt { 1u32 << 31 } else { 0 };
            let prdt = AhciPrdtEntry {
                dba: page_phys as u32,
                dbau: (page_phys >> 32) as u32,
                reserved: 0,
                dbc: ((chunk_len - 1) as u32) | ioc,
            };
            unsafe {
                let prdt_ptr = cmd_tbl_ptr.add(0x80).cast::<AhciPrdtEntry>().add(i);
                ptr::write_volatile(prdt_ptr, prdt);
            }
        }

        fence(Ordering::SeqCst);
        self.port_write32(PORT_REG_CI, 1);

        let start = self.hal.micros();
        for _ in 0..MAX_POLL_YIELDS {
            let ci = self.port_read32(PORT_REG_CI);
            let is = self.port_read32(PORT_REG_IS);
            let tfd = self.port_read32(PORT_REG_TFD);

            if (is & PORT_IS_ERRORS) != 0 || ((tfd & 0x80) == 0 && (tfd & 1) != 0) {
                let _ = self.recover_port().await;
                return Err(Error::from(format!(
                    "AHCI cmd {cmd:#X} failed: TFD={tfd:#X}, IS={is:#X}"
                )));
            }

            if (ci & 1) == 0 {
                self.port_write32(PORT_REG_IS, is);
                if (tfd & 0x80) == 0 && (tfd & 1) != 0 {
                    let _ = self.recover_port().await;
                    return Err(Error::from(format!(
                        "AHCI cmd {cmd:#X} error: TFD={tfd:#X}, IS={is:#X}"
                    )));
                }
                return Ok(());
            }

            if self.hal.micros().saturating_sub(start) > COMMAND_TIMEOUT_MICROS {
                let _ = self.recover_port().await;
                return Err(Error::from(format!("AHCI cmd {cmd:#X} timed out")));
            }
            self.hal.yield_now().await;
        }

        let _ = self.recover_port().await;
        Err(Error::from(format!("AHCI cmd {cmd:#X} timed out")))
    }

    async fn issue_cmd(
        &mut self,
        cmd: u8,
        write: bool,
        lba: u64,
        count: u32,
        byte_count: usize,
    ) -> Result<()> {
        match self
            .issue_cmd_once(cmd, write, lba, count, byte_count)
            .await
        {
            Ok(()) => Ok(()),
            Err(_first_err) => {
                self.issue_cmd_once(cmd, write, lba, count, byte_count)
                    .await
            }
        }
    }

    async fn identify(&mut self) -> Result<()> {
        unsafe {
            self.data.as_mut_slice()[..512].fill(0);
        }
        self.issue_cmd(ATA_CMD_IDENTIFY, false, 0, 0, 512).await?;

        let bytes = unsafe { &self.data.as_slice()[..512] };
        let word83 = u16::from_le_bytes(bytes[166..168].try_into().unwrap());
        let supports_lba48 = (word83 & (1 << 10)) != 0;
        let sectors_48 = u64::from_le_bytes(bytes[200..208].try_into().unwrap());
        let sectors_28 = u32::from_le_bytes(bytes[120..124].try_into().unwrap()) as u64;

        if (supports_lba48 && sectors_48 > 0) || (sectors_48 > 0 && sectors_28 == 0) {
            self.lba48 = true;
            self.blocks = sectors_48;
        } else {
            self.lba48 = false;
            self.blocks = sectors_28.min(0x0FFF_FFFF);
        }

        let word106 = u16::from_le_bytes(bytes[212..214].try_into().unwrap());
        if (word106 & 0xC000) == 0x4000 && (word106 & (1 << 12)) != 0 {
            let lss = u32::from_le_bytes(bytes[234..238].try_into().unwrap()) * 2;
            if (512..=PAGE_SIZE as u32).contains(&lss) && lss.is_power_of_two() {
                self.block_size = lss;
            } else {
                self.block_size = 512;
            }
        } else {
            self.block_size = 512;
        }

        if self.blocks == 0 {
            return Err(Error::msg("AHCI SATA drive reported 0 blocks"));
        }
        Ok(())
    }

    pub fn size(&self) -> u64 {
        (self.block_size as u64).saturating_mul(self.blocks)
    }

    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    pub fn num_blocks(&self) -> u64 {
        self.blocks
    }

    pub fn is_lba48(&self) -> bool {
        self.lba48
    }

    /// Asynchronously reads `buf.len()` bytes starting at byte `offset`.
    /// Uses scatter-gather PRDT entries (up to 256 KiB per command on LBA48, 128 KiB on LBA28)
    /// and retries transient errors with port recovery.
    pub async fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        read_bytes(
            self,
            offset,
            buf,
            "AHCI read offset overflow",
            "AHCI read out of bounds",
        )
        .await
    }

    /// Asynchronously writes `buf.len()` bytes starting at byte `offset`.
    pub async fn write(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        write_bytes(
            self,
            offset,
            buf,
            "AHCI write offset overflow",
            "AHCI write out of bounds",
        )
        .await
    }

    /// Flushes the drive write cache (`FLUSH CACHE EXT` / `FLUSH CACHE`).
    pub async fn flush(&mut self) -> Result<()> {
        let cmd = if self.lba48 {
            ATA_CMD_FLUSH_EXT
        } else {
            ATA_CMD_FLUSH
        };
        self.issue_cmd(cmd, false, 0, 0, 0).await
    }
}

impl<H: Hal> BlockDeviceIo for AhciDisk<H> {
    fn total_bytes(&self) -> u64 {
        self.size()
    }

    fn block_bytes(&self) -> usize {
        self.block_size as usize
    }

    fn max_transfer_blocks(&self) -> usize {
        let proto_max = if self.lba48 { 65536 } else { 256 };
        ((AHCI_MAX_DMA_PAGES * PAGE_SIZE) / (self.block_size as usize))
            .min(proto_max)
            .max(1)
    }

    fn dma_slice(&self) -> &[u8] {
        unsafe { self.data.as_slice() }
    }

    fn dma_mut_slice(&mut self) -> &mut [u8] {
        unsafe { self.data.as_mut_slice() }
    }

    async fn transfer_blocks(
        &mut self,
        write: bool,
        lba: u64,
        blocks: u32,
        transfer_bytes: usize,
    ) -> Result<()> {
        let cmd = match (write, self.lba48) {
            (false, true) => ATA_CMD_READ_DMA_EXT,
            (false, false) => ATA_CMD_READ_DMA,
            (true, true) => ATA_CMD_WRITE_DMA_EXT,
            (true, false) => ATA_CMD_WRITE_DMA,
        };
        self.issue_cmd(cmd, write, lba, blocks, transfer_bytes)
            .await
    }
}

impl<H: Hal> Drop for AhciDisk<H> {
    fn drop(&mut self) {
        // Stop PxCMD.ST (bit 0) and PxCMD.FRE (bit 4) before releasing DMA pages.
        let cmd = self.port_read32(PORT_REG_CMD);
        if cmd != 0xFFFF_FFFF {
            let masked = cmd & 0x0FFF_FFFF;
            if (masked & 1) != 0 {
                self.port_write32(PORT_REG_CMD, masked & !1);
                let start = self.hal.micros();
                for _ in 0..2_000 {
                    if (self.port_read32(PORT_REG_CMD) & (1 << 15)) == 0 {
                        break;
                    }
                    if self.hal.micros().saturating_sub(start) > 50_000 {
                        break;
                    }
                }
            }
            let masked = self.port_read32(PORT_REG_CMD) & 0x0FFF_FFFF;
            if (masked & (1 << 4)) != 0 {
                self.port_write32(PORT_REG_CMD, masked & !((1 << 4) | 1));
                let start = self.hal.micros();
                for _ in 0..2_000 {
                    if (self.port_read32(PORT_REG_CMD) & (1 << 14)) == 0 {
                        break;
                    }
                    if self.hal.micros().saturating_sub(start) > 50_000 {
                        break;
                    }
                }
            }
        }
        fence(Ordering::SeqCst);
        self.hal.free_dma(self.ctrl);
        self.hal.free_dma(self.data);
    }
}
