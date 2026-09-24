use alloc::format;
use alloc::vec::Vec;
use core::ptr::{self, NonNull};
use core::sync::atomic::{Ordering, fence};

use crate::os::error::{Error, Result};
use crate::os::executor::Executor;
use crate::os::memory::{self, PAGE_SIZE};
use crate::os::pci::{PciDevice, scan_pci};
use crate::os::timer::Timer;

const ATA_CMD_READ_DMA: u8 = 0xC8;
const ATA_CMD_WRITE_DMA: u8 = 0xCA;
const ATA_CMD_FLUSH: u8 = 0xE7;
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

const PORT_IS_TFES: u32 = 1 << 30; // Task File Error Status
const PORT_IS_HBFS: u32 = 1 << 29; // Host Bus Fatal Error Status
const PORT_IS_HBDS: u32 = 1 << 28; // Host Bus Data Error Status
const PORT_IS_IFS: u32 = 1 << 27;  // Interface Fatal Error Status
const PORT_IS_OFS: u32 = 1 << 24;  // Overflow Status
const PORT_IS_ERRORS: u32 =
    PORT_IS_TFES | PORT_IS_HBFS | PORT_IS_HBDS | PORT_IS_IFS | PORT_IS_OFS;

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
    lba48: bool,
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

    fn bios_handoff(abar: u64) {
        let cap2 = Self::read32(abar, 0x24);
        // Bit 0 of CAP2: BIOS/OS Handoff (BOH)
        if (cap2 & 1) != 0 {
            let bohc = Self::read32(abar, 0x28);
            // Bit 0: BIOS Owned Semaphore (BOS)
            if (bohc & 1) != 0 {
                log::info!("AHCI: BIOS owns controller (BOS=1), performing OS handoff...");
                // Set Bit 1: OS Owned Semaphore (OOS)
                Self::write32(abar, 0x28, bohc | (1 << 1));

                // Wait up to 2 seconds for BOS to clear
                let deadline = Timer::micros() + 2_000_000;
                while (Self::read32(abar, 0x28) & 1) != 0 {
                    if Timer::micros() > deadline {
                        log::warn!("AHCI: BIOS/OS handoff timed out waiting for BOS=0");
                        break;
                    }
                    crate::os::arch::io::pause();
                }

                let bohc = Self::read32(abar, 0x28);
                if (bohc & (1 << 4)) != 0 {
                    let deadline = Timer::micros() + 2_000_000;
                    while (Self::read32(abar, 0x28) & (1 << 4)) != 0 {
                        if Timer::micros() > deadline {
                            log::warn!("AHCI: BIOS/OS handoff timed out waiting for BB=0");
                            break;
                        }
                        crate::os::arch::io::pause();
                    }
                }
                log::info!("AHCI: BIOS/OS handoff complete");
            }
        }
    }

    pub async fn open(pci: PciDevice, port_idx: usize) -> Result<Self> {
        pci.enable_bus_mastering();
        let abar = match pci.read_bar(5) {
            Some(crate::os::pci::Bar::Memory { base, .. }) => base,
            _ => return Err(Error::msg("AHCI ABAR (BAR5) is not memory-mapped")),
        };

        Self::bios_handoff(abar);

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
            lba48: false,
        };

        disk.init_port()?;
        disk.identify().await?;
        Ok(disk)
    }

    fn recover_port(&self) {
        // Clear ST to halt command processing
        let cmd = self.port_read32(PORT_REG_CMD) & 0x0FFF_FFFF;
        self.port_write32(PORT_REG_CMD, cmd & !1);

        // Wait for CR to clear to 0
        let deadline = Timer::micros() + 500_000;
        while (self.port_read32(PORT_REG_CMD) & (1 << 15)) != 0 {
            if Timer::micros() > deadline {
                log::warn!("AHCI recover_port: CR failed to clear to 0");
                break;
            }
            crate::os::arch::io::pause();
        }

        // Clear error and interrupt status
        self.port_write32(PORT_REG_SERR, 0xFFFF_FFFF);
        self.port_write32(PORT_REG_IS, 0xFFFF_FFFF);

        // Re-enable ST
        let cmd = self.port_read32(PORT_REG_CMD) & 0x0FFF_FFFF;
        self.port_write32(PORT_REG_CMD, cmd | 1);
    }

    fn stop_port(&self) -> Result<()> {
        self.port_write32(PORT_REG_SERR, 0xFFFF_FFFF);
        self.port_write32(PORT_REG_IS, 0xFFFF_FFFF);

        // 1. Clear ST
        let cmd = self.port_read32(PORT_REG_CMD) & 0x0FFF_FFFF;
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
        let cmd = self.port_read32(PORT_REG_CMD) & 0x0FFF_FFFF;
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
        // Wait for CR to clear to 0 before starting
        let deadline = Timer::micros() + 500_000;
        while (self.port_read32(PORT_REG_CMD) & (1 << 15)) != 0 {
            if Timer::micros() > deadline {
                log::warn!("AHCI port start timeout waiting for CR=0");
                break;
            }
            crate::os::arch::io::pause();
        }

        // Spin-up device (bit 1: SUD)
        let cmd = self.port_read32(PORT_REG_CMD) & 0x0FFF_FFFF;
        self.port_write32(PORT_REG_CMD, cmd | (1 << 1));

        // Enable FIS receive (bit 4: FRE)
        let cmd = self.port_read32(PORT_REG_CMD) & 0x0FFF_FFFF;
        self.port_write32(PORT_REG_CMD, cmd | (1 << 4));

        // AHCI Spec 10.3.1: Wait for FR (bit 14) to become 1 before setting ST!
        let deadline = Timer::micros() + 500_000;
        while (self.port_read32(PORT_REG_CMD) & (1 << 14)) == 0 {
            if Timer::micros() > deadline {
                log::warn!("AHCI port start timeout waiting for FR=1");
                break;
            }
            crate::os::arch::io::pause();
        }

        // Clear error and interrupt status before starting command engine
        self.port_write32(PORT_REG_SERR, 0xFFFF_FFFF);
        self.port_write32(PORT_REG_IS, 0xFFFF_FFFF);

        // Start command engine (bit 0: ST)
        let cmd = self.port_read32(PORT_REG_CMD) & 0x0FFF_FFFF;
        self.port_write32(PORT_REG_CMD, cmd | 1);
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
        // 1. Wait for port to not be busy (BSY == 0 and DRQ == 0)
        let deadline = Timer::micros() + 1_000_000;
        while (self.port_read32(PORT_REG_TFD) & 0x88) != 0 {
            if Timer::micros() > deadline {
                let tfd = self.port_read32(PORT_REG_TFD);
                let ssts = self.port_read32(PORT_REG_SSTS);
                let is = self.port_read32(PORT_REG_IS);
                let serr = self.port_read32(PORT_REG_SERR);
                self.recover_port();
                return Err(Error::msg(format!(
                    "AHCI port busy before cmd {cmd:#X}: TFD={tfd:#X}, IS={is:#X}, SERR={serr:#X}, SSTS={ssts:#X}"
                )));
            }
            crate::os::arch::io::pause();
        }

        // 2. Clear any lingering interrupt and error status
        self.port_write32(PORT_REG_SERR, 0xFFFF_FFFF);
        self.port_write32(PORT_REG_IS, 0xFFFF_FFFF);

        // 3. Setup Command Header 0
        let has_prdt = byte_count > 0;
        let hdr = AhciCmdHeader {
            flags: 5 | (if write { 1 << 6 } else { 0 }),
            prdtl: if has_prdt { 1 } else { 0 },
            prdbc: 0,
            ctba: self.cmd_table.phys() as u32,
            ctbau: (self.cmd_table.phys() >> 32) as u32,
            reserved: [0; 4],
        };
        unsafe {
            ptr::write_volatile(self._cmd_list.as_ptr::<AhciCmdHeader>(), hdr);
        }

        // 4. Setup Command Table
        let cmd_tbl_ptr = self.cmd_table.as_ptr::<u8>();
        unsafe {
            ptr::write_bytes(cmd_tbl_ptr, 0, 128); // Clear Command FIS
        }

        // 5. Setup Command FIS (Register H2D)
        let fis = unsafe { core::slice::from_raw_parts_mut(cmd_tbl_ptr, 20) };
        fis[0] = 0x27; // FIS Type: Register FIS H2D
        fis[1] = 0x80; // C = 1 (Command)
        fis[2] = cmd;  // ATA command opcode
        if cmd == ATA_CMD_IDENTIFY {
            fis[7] = 0;
        } else if self.lba48 {
            fis[4] = (lba & 0xFF) as u8;
            fis[5] = ((lba >> 8) & 0xFF) as u8;
            fis[6] = ((lba >> 16) & 0xFF) as u8;
            fis[7] = 1 << 6; // LBA mode
            fis[8] = ((lba >> 24) & 0xFF) as u8;
            fis[9] = ((lba >> 32) & 0xFF) as u8;
            fis[10] = ((lba >> 40) & 0xFF) as u8;
            fis[12] = (count & 0xFF) as u8;
            fis[13] = ((count >> 8) & 0xFF) as u8;
        } else {
            fis[4] = (lba & 0xFF) as u8;
            fis[5] = ((lba >> 8) & 0xFF) as u8;
            fis[6] = ((lba >> 16) & 0xFF) as u8;
            fis[7] = (0xE0 | ((lba >> 24) & 0x0F)) as u8;
            fis[12] = (count & 0xFF) as u8;
        }

        // 6. Setup PRDT Entry 0 if data transfer is required
        if has_prdt {
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
        }

        fence(Ordering::Release);

        // 7. Issue command on slot 0
        self.port_write32(PORT_REG_CI, 1);

        // Fast path: brief spin for ultra-fast completions (< 50 us)
        for _ in 0..100 {
            let ci = self.port_read32(PORT_REG_CI);
            let is = self.port_read32(PORT_REG_IS);
            let tfd = self.port_read32(PORT_REG_TFD);

            if (ci & 1) == 0 {
                self.port_write32(PORT_REG_IS, is);
                if (tfd & 1) != 0 {
                    let serr = self.port_read32(PORT_REG_SERR);
                    self.recover_port();
                    return Err(Error::msg(format!(
                        "AHCI cmd {cmd:#X} error (TFD ERR): TFD={tfd:#X}, IS={is:#X}, SERR={serr:#X}"
                    )));
                }
                return Ok(());
            }

            if (is & PORT_IS_ERRORS) != 0 || (tfd & 1) != 0 {
                let serr = self.port_read32(PORT_REG_SERR);
                self.recover_port();
                return Err(Error::msg(format!(
                    "AHCI cmd {cmd:#X} failed: TFD={tfd:#X}, IS={is:#X}, SERR={serr:#X}"
                )));
            }
            core::hint::spin_loop();
        }

        // Async path: cooperative sleep allowing executor to run other tasks or halt CPU
        let deadline = Timer::micros() + COMMAND_TIMEOUT_MICROS;
        loop {
            let ci = self.port_read32(PORT_REG_CI);
            let is = self.port_read32(PORT_REG_IS);
            let tfd = self.port_read32(PORT_REG_TFD);

            if (ci & 1) == 0 {
                self.port_write32(PORT_REG_IS, is);
                if (tfd & 1) != 0 {
                    let serr = self.port_read32(PORT_REG_SERR);
                    self.recover_port();
                    return Err(Error::msg(format!(
                        "AHCI cmd {cmd:#X} error (TFD ERR): TFD={tfd:#X}, IS={is:#X}, SERR={serr:#X}"
                    )));
                }
                return Ok(());
            }

            if (is & PORT_IS_ERRORS) != 0 || (tfd & 1) != 0 {
                let serr = self.port_read32(PORT_REG_SERR);
                self.recover_port();
                return Err(Error::msg(format!(
                    "AHCI cmd {cmd:#X} failed: TFD={tfd:#X}, IS={is:#X}, SERR={serr:#X}"
                )));
            }

            if Timer::micros() > deadline {
                let serr = self.port_read32(PORT_REG_SERR);
                let ssts = self.port_read32(PORT_REG_SSTS);
                let cmd_reg = self.port_read32(PORT_REG_CMD);
                self.recover_port();
                return Err(Error::msg(format!(
                    "AHCI cmd {cmd:#X} timeout: CI={ci:#X}, TFD={tfd:#X}, IS={is:#X}, SERR={serr:#X}, SSTS={ssts:#X}, CMD={cmd_reg:#X}"
                )));
            }
            Executor::sched_yield().await;
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
        let deadline = Timer::micros() + 1_000_000;
        while (self.port_read32(PORT_REG_TFD) & 0x88) != 0 {
            if Timer::micros() > deadline {
                let tfd = self.port_read32(PORT_REG_TFD);
                let ssts = self.port_read32(PORT_REG_SSTS);
                let is = self.port_read32(PORT_REG_IS);
                let serr = self.port_read32(PORT_REG_SERR);
                self.recover_port();
                return Err(Error::msg(format!(
                    "AHCI port busy before cmd {cmd:#X} (sync): TFD={tfd:#X}, IS={is:#X}, SERR={serr:#X}, SSTS={ssts:#X}"
                )));
            }
            crate::os::arch::io::pause();
        }

        self.port_write32(PORT_REG_SERR, 0xFFFF_FFFF);
        self.port_write32(PORT_REG_IS, 0xFFFF_FFFF);

        let has_prdt = byte_count > 0;
        let hdr = AhciCmdHeader {
            flags: 5 | (if write { 1 << 6 } else { 0 }),
            prdtl: if has_prdt { 1 } else { 0 },
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
        if cmd == ATA_CMD_IDENTIFY {
            fis[7] = 0;
        } else if self.lba48 {
            fis[4] = (lba & 0xFF) as u8;
            fis[5] = ((lba >> 8) & 0xFF) as u8;
            fis[6] = ((lba >> 16) & 0xFF) as u8;
            fis[7] = 1 << 6;
            fis[8] = ((lba >> 24) & 0xFF) as u8;
            fis[9] = ((lba >> 32) & 0xFF) as u8;
            fis[10] = ((lba >> 40) & 0xFF) as u8;
            fis[12] = (count & 0xFF) as u8;
            fis[13] = ((count >> 8) & 0xFF) as u8;
        } else {
            fis[4] = (lba & 0xFF) as u8;
            fis[5] = ((lba >> 8) & 0xFF) as u8;
            fis[6] = ((lba >> 16) & 0xFF) as u8;
            fis[7] = (0xE0 | ((lba >> 24) & 0x0F)) as u8;
            fis[12] = (count & 0xFF) as u8;
        }

        if has_prdt {
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
        }

        fence(Ordering::Release);

        self.port_write32(PORT_REG_CI, 1);

        let deadline = Timer::micros() + COMMAND_TIMEOUT_MICROS;
        loop {
            let ci = self.port_read32(PORT_REG_CI);
            let is = self.port_read32(PORT_REG_IS);
            let tfd = self.port_read32(PORT_REG_TFD);

            if (ci & 1) == 0 {
                self.port_write32(PORT_REG_IS, is);
                if (tfd & 1) != 0 {
                    let serr = self.port_read32(PORT_REG_SERR);
                    self.recover_port();
                    return Err(Error::msg(format!(
                        "AHCI cmd {cmd:#X} error (TFD ERR sync): TFD={tfd:#X}, IS={is:#X}, SERR={serr:#X}"
                    )));
                }
                return Ok(());
            }

            if (is & PORT_IS_ERRORS) != 0 || (tfd & 1) != 0 {
                let serr = self.port_read32(PORT_REG_SERR);
                self.recover_port();
                return Err(Error::msg(format!(
                    "AHCI cmd {cmd:#X} failed (sync): TFD={tfd:#X}, IS={is:#X}, SERR={serr:#X}"
                )));
            }

            if Timer::micros() > deadline {
                let serr = self.port_read32(PORT_REG_SERR);
                let ssts = self.port_read32(PORT_REG_SSTS);
                let cmd_reg = self.port_read32(PORT_REG_CMD);
                self.recover_port();
                return Err(Error::msg(format!(
                    "AHCI cmd {cmd:#X} timeout (sync): CI={ci:#X}, TFD={tfd:#X}, IS={is:#X}, SERR={serr:#X}, SSTS={ssts:#X}, CMD={cmd_reg:#X}"
                )));
            }
            crate::os::arch::io::pause();
        }
    }

    async fn identify(&mut self) -> Result<()> {
        let phys = self.data.phys();
        self.issue_cmd(ATA_CMD_IDENTIFY, false, 0, 0, phys, 512)
            .await?;

        let bytes = self.data.as_bytes();
        let word83 = u16::from_le_bytes(bytes[166..168].try_into().unwrap());
        let supports_lba48 = (word83 & (1 << 10)) != 0 && (word83 & (1 << 14)) != 0;
        let sectors_48 = u64::from_le_bytes(bytes[200..208].try_into().unwrap());
        let sectors_28 = u32::from_le_bytes(bytes[120..124].try_into().unwrap()) as u64;

        if supports_lba48 && sectors_48 > 0 {
            self.lba48 = true;
            self.blocks = sectors_48;
        } else {
            self.lba48 = false;
            self.blocks = sectors_28;
        }
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
        let cmd = if self.lba48 {
            ATA_CMD_FLUSH_EXT
        } else {
            ATA_CMD_FLUSH
        };
        self.issue_cmd(cmd, false, 0, 0, 0, 0).await
    }

    pub fn read_sync(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let mut cur_offset = offset;
        let mut remaining = buf;
        let max_blocks = (self.data.pages * PAGE_SIZE as usize) / self.block_size as usize;
        let cmd = if self.lba48 {
            ATA_CMD_READ_DMA_EXT
        } else {
            ATA_CMD_READ_DMA
        };
        while !remaining.is_empty() {
            let lba = cur_offset / self.block_size;
            let in_block = (cur_offset % self.block_size) as usize;
            let blocks = ((in_block + remaining.len()).div_ceil(self.block_size as usize))
                .min(max_blocks);

            let phys = self.data.phys();
            self.issue_cmd_sync(
                cmd,
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
        let cmd = if self.lba48 {
            ATA_CMD_READ_DMA_EXT
        } else {
            ATA_CMD_READ_DMA
        };
        while !remaining.is_empty() {
            let lba = cur_offset / self.block_size;
            let in_block = (cur_offset % self.block_size) as usize;
            let blocks = ((in_block + remaining.len()).div_ceil(self.block_size as usize))
                .min(max_blocks);

            let phys = self.data.phys();
            self.issue_cmd(
                cmd,
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
        let read_cmd = if self.lba48 {
            ATA_CMD_READ_DMA_EXT
        } else {
            ATA_CMD_READ_DMA
        };
        let write_cmd = if self.lba48 {
            ATA_CMD_WRITE_DMA_EXT
        } else {
            ATA_CMD_WRITE_DMA
        };
        while !remaining.is_empty() {
            let lba = cur_offset / self.block_size;
            let in_block = (cur_offset % self.block_size) as usize;
            let blocks = ((in_block + remaining.len()).div_ceil(self.block_size as usize))
                .min(max_blocks);

            let phys = self.data.phys();
            if in_block != 0 || remaining.len() < blocks * self.block_size as usize {
                self.issue_cmd_sync(
                    read_cmd,
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
                write_cmd,
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
        let read_cmd = if self.lba48 {
            ATA_CMD_READ_DMA_EXT
        } else {
            ATA_CMD_READ_DMA
        };
        let write_cmd = if self.lba48 {
            ATA_CMD_WRITE_DMA_EXT
        } else {
            ATA_CMD_WRITE_DMA
        };
        while !remaining.is_empty() {
            let lba = cur_offset / self.block_size;
            let in_block = (cur_offset % self.block_size) as usize;
            let blocks = ((in_block + remaining.len()).div_ceil(self.block_size as usize))
                .min(max_blocks);

            let phys = self.data.phys();
            if in_block != 0 || remaining.len() < blocks * self.block_size as usize {
                self.issue_cmd(
                    read_cmd,
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
                write_cmd,
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

                Self::bios_handoff(abar);

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
                                    "SATA drive initialized: {} blocks of {} bytes ({} GB, LBA48={})",
                                    disk.blocks,
                                    disk.block_size,
                                    disk.size() / (1024 * 1024 * 1024),
                                    disk.lba48
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

impl Drop for AhciDisk {
    fn drop(&mut self) {
        let _ = self.stop_port();
    }
}
