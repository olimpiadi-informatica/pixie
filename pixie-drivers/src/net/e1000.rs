use core::ptr;
use core::sync::atomic::{Ordering, fence};

use crate::error::{Error, Result};
use crate::hal::{DmaBuffer, Hal, PAGE_SIZE};
use crate::pci::PciDevice;

pub const NUM_RX_DESC: usize = 512;
pub const NUM_TX_DESC: usize = 256;
pub const BUFFER_SIZE: usize = 2048;

const REG_CTRL: u64 = 0x0000;
const REG_STATUS: u64 = 0x0008;
const REG_EERD: u64 = 0x0014;
const REG_ICR: u64 = 0x00C0;
const REG_IMC: u64 = 0x00D8;
const REG_RCTL: u64 = 0x0100;
const REG_TCTL: u64 = 0x0400;
const REG_TIPG: u64 = 0x0410;
const REG_EXTCNF_CTRL: u64 = 0x0F00;
const REG_RDBAL: u64 = 0x2800;
const REG_RDBAH: u64 = 0x2804;
const REG_RDLEN: u64 = 0x2808;
const REG_RDH: u64 = 0x2810;
const REG_RDT: u64 = 0x2818;
const REG_RXDCTL: u64 = 0x2828;
const REG_TDBAL: u64 = 0x3800;
const REG_TDBAH: u64 = 0x3804;
const REG_TDLEN: u64 = 0x3808;
const REG_TDH: u64 = 0x3810;
const REG_TDT: u64 = 0x3818;
const REG_TXDCTL: u64 = 0x3828;
const REG_TARC0: u64 = 0x3840;
const REG_CRCERRS: u64 = 0x4000;
const REG_MPC: u64 = 0x4010;
const REG_GPRC: u64 = 0x4074;
const REG_GPTC: u64 = 0x4080;
const REG_RNBC: u64 = 0x40A0;
const REG_RFCTL: u64 = 0x5008;
const REG_MTA: u64 = 0x5200;
const REG_RAL: u64 = 0x5400;
const REG_RAH: u64 = 0x5404;
const REG_MRQC: u64 = 0x5818;
const REG_MANC: u64 = 0x5820;
const REG_MANC2H: u64 = 0x5860;
const REG_FWSM: u64 = 0x5B54;
const REG_FEXTNVM11: u64 = 0x5BBC;

const CTRL_SLU: u32 = 1 << 6;
const CTRL_RST: u32 = 1 << 26;

const RFCTL_EXSTEN: u32 = 1 << 15;
const MANC_EN_MNG2HOST: u32 = 1 << 21;
const MANC_ARP_EN: u32 = 1 << 13;
const FWSM_PCIM2PCI: u32 = 1 << 24;
const FEXTNVM11_DISABLE_MULR_FIX: u32 = 1 << 13;

const RCTL_EN: u32 = 1 << 1;
const RCTL_UPE: u32 = 1 << 3;
const RCTL_MPE: u32 = 1 << 4;
const RCTL_BAM: u32 = 1 << 15;
const RCTL_SZ_2048: u32 = 0 << 16;
const RCTL_SECRC: u32 = 1 << 26;

const RXDCTL_ENABLE: u32 = 1 << 25;

const TCTL_EN: u32 = 1 << 1;
const TCTL_PSP: u32 = 1 << 3;
const TCTL_CT_SHIFT: u32 = 4;
const TCTL_COLD_SHIFT: u32 = 12;
const TCTL_MULR: u32 = 1 << 28;

const TXDCTL_PTHRESH_31: u32 = 0x1F;
const TXDCTL_HTHRESH_1: u32 = 1 << 8;
const TXDCTL_WTHRESH_1: u32 = 1 << 16;
const TXDCTL_COUNT_DESC: u32 = 1 << 22;
const TXDCTL_GRAN: u32 = 1 << 24;
const TXDCTL_ENABLE: u32 = 1 << 25;
const TXDCTL_DMA_BURST: u32 =
    TXDCTL_PTHRESH_31 | TXDCTL_HTHRESH_1 | TXDCTL_WTHRESH_1 | TXDCTL_COUNT_DESC | TXDCTL_GRAN;

const CMD_EOP: u8 = 1 << 0;
const CMD_IFCS: u8 = 1 << 1;
const CMD_RS: u8 = 1 << 3;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RxDesc {
    pub buffer_addr: u64,
    pub length: u16,
    pub checksum: u16,
    pub status: u8,
    pub errors: u8,
    pub special: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TxDesc {
    pub buffer_addr: u64,
    pub length: u16,
    pub cso: u8,
    pub cmd: u8,
    pub status: u8,
    pub css: u8,
    pub special: u16,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct E1000Stats {
    pub gprc: u32,
    pub gptc: u32,
    pub mpc: u32,
    pub rnbc: u32,
    pub crcerrs: u32,
    pub rdh: u32,
    pub rdt: u32,
    pub tdh: u32,
    pub tdt: u32,
    pub status: u32,
    pub tctl: u32,
    pub txdctl: u32,
    pub rctl: u32,
    pub rxdctl: u32,
}

pub struct E1000Device<H: Hal> {
    pub pci: PciDevice,
    pub mmio_base: u64,
    pub hal: H,
    mac: [u8; 6],
    rx_ring: DmaBuffer,
    rx_bufs: DmaBuffer,
    rx_cur: usize,
    tx_ring: DmaBuffer,
    tx_bufs: DmaBuffer,
    tx_cur: usize,
    tx_clean: usize,
}

impl<H: Hal> E1000Device<H> {
    fn mmio_read(&self, offset: u64) -> u32 {
        self.hal.mmio_read32(self.mmio_base + offset)
    }

    fn mmio_write(&self, offset: u64, val: u32) {
        self.hal.mmio_write32(self.mmio_base + offset, val);
    }

    fn safe_write_tail(hal: &H, base: u64, reg: u64, val: u32) {
        for _ in 0..2000 {
            let fwsm = hal.mmio_read32(base + REG_FWSM);
            if fwsm == 0xFFFF_FFFF || (fwsm & FWSM_PCIM2PCI) == 0 {
                break;
            }
        }
        hal.mmio_write32(base + reg, val);
        for _ in 0..1000 {
            if hal.mmio_read32(base + reg) == val {
                return;
            }
            hal.mmio_write32(base + reg, val);
        }
    }

    fn read_eeprom(hal: &H, base: u64, addr: u8) -> u16 {
        // e1000 format: start=bit 0, addr=bits 8..15, done=bit 4, data=bits 16..31
        hal.mmio_write32(base + REG_EERD, 1 | ((addr as u32) << 8));
        for _ in 0..1_000 {
            let val = hal.mmio_read32(base + REG_EERD);
            if val != 0xFFFF_FFFF && (val & (1 << 4)) != 0 {
                return (val >> 16) as u16;
            }
        }

        // e1000e format: start=bit 0, addr=bits 2..15, done=bit 1, data=bits 16..31
        hal.mmio_write32(base + REG_EERD, 1 | ((addr as u32) << 2));
        for _ in 0..1_000 {
            let val = hal.mmio_read32(base + REG_EERD);
            if val != 0xFFFF_FFFF && (val & (1 << 1)) != 0 {
                return (val >> 16) as u16;
            }
        }
        0
    }

    /// Initializes an Intel e1000 / e1000e (82540EM, 82574L, I217, I218, I219) Ethernet controller:
    /// - Reads MAC from RAL/RAH, falling back to EEPROM (`EERD`) and then `fallback_mac`.
    /// - Disables interrupts (`IMC = 0xFFFF_FFFF`), stops RX/TX cleanly without asserting destructive `CTRL.RST`.
    /// - Clears `RFCTL` (`0x5008`, disabling `EXSTEN` extended descriptors) and `MRQC` (`0x5818`, disabling multi-queue RSS).
    /// - Configures `MANC` (`0x5820`, setting `EN_MNG2HOST` and clearing `ARP_EN`) and `FEXTNVM11` (`0x5BBC`).
    /// - Initializes RX ring in strict order: `RCTL=0` -> program `RDBAL/H`, `RDLEN`, `RDH=0`, `RDT=0` ->
    ///   enable `RXDCTL` (`ENABLE | PTHRESH(16) | HTHRESH(4) | WTHRESH(1)`) -> enable `RCTL` -> write `RDT = NUM_RX_DESC - 1`
    ///   (waiting for `FWSM.PCIM2PCI == 0` and verifying tail writeback).
    /// - Initializes TX ring (`TXDCTL` with `PTHRESH/HTHRESH/GRAN/ENABLE`, `TCTL` with `EN | PSP`, clearing `MULR`).
    pub fn new(
        pci: PciDevice,
        mmio_base: u64,
        fallback_mac: Option<[u8; 6]>,
        hal: H,
    ) -> Result<Self> {
        if mmio_base == 0 {
            return Err(Error::msg("E1000 MMIO base is 0"));
        }
        pci.enable_bus_mastering(&hal);
        hal.map_mmio(mmio_base, 0x20000);

        // Release PCH EXTCNF_CTRL SWFLAG semaphore if held
        let extcnf = hal.mmio_read32(mmio_base + REG_EXTCNF_CTRL);
        if extcnf != 0xFFFF_FFFF && (extcnf & (1 << 5)) != 0 {
            hal.mmio_write32(mmio_base + REG_EXTCNF_CTRL, extcnf & !(1 << 5));
        }

        // 1. Read MAC address from RAL/RAH, falling back to EEPROM (EERD) and then fallback_mac
        let is_valid_mac = |m: &[u8; 6]| *m != [0; 6] && *m != [0xFF; 6] && (m[0] & 1) == 0;

        let mut mac = [0u8; 6];
        let ral = hal.mmio_read32(mmio_base + REG_RAL);
        let rah = hal.mmio_read32(mmio_base + REG_RAH);
        if ral != 0 && ral != 0xFFFF_FFFF {
            let candidate = [
                (ral & 0xFF) as u8,
                ((ral >> 8) & 0xFF) as u8,
                ((ral >> 16) & 0xFF) as u8,
                ((ral >> 24) & 0xFF) as u8,
                (rah & 0xFF) as u8,
                ((rah >> 8) & 0xFF) as u8,
            ];
            if is_valid_mac(&candidate) {
                mac = candidate;
            }
        }

        if !is_valid_mac(&mac) {
            let mut eeprom_mac = [0u8; 6];
            for i in 0..3 {
                let word = Self::read_eeprom(&hal, mmio_base, i as u8);
                eeprom_mac[i * 2] = (word & 0xFF) as u8;
                eeprom_mac[i * 2 + 1] = ((word >> 8) & 0xFF) as u8;
            }
            if is_valid_mac(&eeprom_mac) {
                mac = eeprom_mac;
            }
        }

        if !is_valid_mac(&mac)
            && let Some(fb) = fallback_mac
        {
            mac = fb;
        }

        // 2. Disable interrupts
        hal.mmio_write32(mmio_base + REG_IMC, 0xFFFF_FFFF);
        let _ = hal.mmio_read32(mmio_base + REG_ICR);

        let tctl_init = hal.mmio_read32(mmio_base + REG_TCTL);
        let tipg_init = hal.mmio_read32(mmio_base + REG_TIPG);

        // 3. Stop RX and TX cleanly without asserting destructive CTRL.RST
        hal.mmio_write32(mmio_base + REG_RCTL, 0);
        hal.mmio_write32(mmio_base + REG_TCTL, 0);
        hal.mmio_write32(mmio_base + REG_RDBAH, 0);
        hal.mmio_write32(mmio_base + REG_RDBAL, 0);
        hal.mmio_write32(mmio_base + REG_RDLEN, 0);
        hal.mmio_write32(mmio_base + REG_RDH, 0);
        Self::safe_write_tail(&hal, mmio_base, REG_RDT, 0);
        hal.mmio_write32(mmio_base + REG_RDBAH + 0x100, 0);
        hal.mmio_write32(mmio_base + REG_RDBAL + 0x100, 0);
        hal.mmio_write32(mmio_base + REG_RDLEN + 0x100, 0);
        hal.mmio_write32(mmio_base + REG_RDH + 0x100, 0);
        Self::safe_write_tail(&hal, mmio_base, REG_RDT + 0x100, 0);
        hal.mmio_write32(mmio_base + REG_RXDCTL + 0x100, 0);
        hal.mmio_write32(mmio_base + REG_TDBAH, 0);
        hal.mmio_write32(mmio_base + REG_TDBAL, 0);
        hal.mmio_write32(mmio_base + REG_TDLEN, 0);
        hal.mmio_write32(mmio_base + REG_TDH, 0);
        Self::safe_write_tail(&hal, mmio_base, REG_TDT, 0);

        hal.mmio_write32(mmio_base + REG_IMC, 0xFFFF_FFFF);
        let _ = hal.mmio_read32(mmio_base + REG_ICR);

        // 4. Clear RFCTL (disabling EXSTEN) and MRQC (disabling multi-queue RSS)
        let rfctl = hal.mmio_read32(mmio_base + REG_RFCTL);
        let new_rfctl = if rfctl == 0xFFFF_FFFF {
            0
        } else {
            rfctl & !RFCTL_EXSTEN
        };
        hal.mmio_write32(mmio_base + REG_RFCTL, new_rfctl);
        hal.mmio_write32(mmio_base + REG_MRQC, 0);

        // 5. Configure FEXTNVM11, TARC0, and MANC
        let fextnvm11 = hal.mmio_read32(mmio_base + REG_FEXTNVM11);
        if fextnvm11 != 0xFFFF_FFFF {
            hal.mmio_write32(
                mmio_base + REG_FEXTNVM11,
                fextnvm11 & !FEXTNVM11_DISABLE_MULR_FIX,
            );
        }
        let tarc0 = hal.mmio_read32(mmio_base + REG_TARC0);
        if tarc0 != 0xFFFF_FFFF {
            let tarc_val = (tarc0 & 0xCFFF_FFFF) | 0x2000_0000;
            hal.mmio_write32(mmio_base + REG_TARC0, tarc_val);
            hal.mmio_write32(mmio_base + REG_TARC0 + 0x100, tarc_val);
        }
        let manc = hal.mmio_read32(mmio_base + REG_MANC);
        if manc != 0xFFFF_FFFF {
            let new_manc = (manc | MANC_EN_MNG2HOST) & !MANC_ARP_EN;
            hal.mmio_write32(mmio_base + REG_MANC, new_manc);
            hal.mmio_write32(mmio_base + REG_MANC2H, 0xFFFF_FFFF);
        }

        // 6. Force link up in CTRL without asserting CTRL_RST
        let ctrl = hal.mmio_read32(mmio_base + REG_CTRL);
        let ctrl_base = if ctrl == 0xFFFF_FFFF { 0 } else { ctrl };
        hal.mmio_write32(mmio_base + REG_CTRL, (ctrl_base & !CTRL_RST) | CTRL_SLU);

        // 7. Clear Multicast Table Array (MTA) and program RAL0/RAH0
        for i in 0..128 {
            hal.mmio_write32(mmio_base + REG_MTA + (i * 4) as u64, 0);
        }
        let ral_val = (mac[0] as u32)
            | ((mac[1] as u32) << 8)
            | ((mac[2] as u32) << 16)
            | ((mac[3] as u32) << 24);
        let rah_val = (mac[4] as u32) | ((mac[5] as u32) << 8) | (1 << 31);
        hal.mmio_write32(mmio_base + REG_RAL, ral_val);
        hal.mmio_write32(mmio_base + REG_RAH, rah_val);

        // 8. Allocate DMA memory for RX and TX rings + buffers
        let rx_ring_bytes = NUM_RX_DESC * core::mem::size_of::<RxDesc>();
        let rx_ring_pages = rx_ring_bytes.div_ceil(PAGE_SIZE);
        let rx_buf_pages = (NUM_RX_DESC * BUFFER_SIZE).div_ceil(PAGE_SIZE);
        let tx_ring_bytes = NUM_TX_DESC * core::mem::size_of::<TxDesc>();
        let tx_ring_pages = tx_ring_bytes.div_ceil(PAGE_SIZE);
        let tx_buf_pages = (NUM_TX_DESC * BUFFER_SIZE).div_ceil(PAGE_SIZE);

        let rx_ring = hal
            .alloc_dma(rx_ring_pages)
            .ok_or_else(|| Error::msg("Out of DMA memory for e1000 RX ring"))?;
        let Some(rx_bufs) = hal.alloc_dma(rx_buf_pages) else {
            hal.free_dma(rx_ring);
            return Err(Error::msg("Out of DMA memory for e1000 RX buffers"));
        };
        let Some(tx_ring) = hal.alloc_dma(tx_ring_pages) else {
            hal.free_dma(rx_ring);
            hal.free_dma(rx_bufs);
            return Err(Error::msg("Out of DMA memory for e1000 TX ring"));
        };
        let Some(tx_bufs) = hal.alloc_dma(tx_buf_pages) else {
            hal.free_dma(rx_ring);
            hal.free_dma(rx_bufs);
            hal.free_dma(tx_ring);
            return Err(Error::msg("Out of DMA memory for e1000 TX buffers"));
        };

        // 9. Initialize RX ring in strict order:
        // RCTL=0 -> program RDBAL/H, RDLEN, RDH=0, RDT=0 ->
        // enable RXDCTL (ENABLE | PTHRESH(16) | HTHRESH(4) | WTHRESH(1)) ->
        // enable RCTL -> write RDT = NUM_RX_DESC - 1
        let rx_descs_ptr = rx_ring.virt as *mut RxDesc;
        for i in 0..NUM_RX_DESC {
            let desc = RxDesc {
                buffer_addr: rx_bufs.phys + (i * BUFFER_SIZE) as u64,
                length: 0,
                checksum: 0,
                status: 0,
                errors: 0,
                special: 0,
            };
            unsafe {
                ptr::write_volatile(rx_descs_ptr.add(i), desc);
            }
        }
        fence(Ordering::SeqCst);

        hal.mmio_write32(mmio_base + REG_RCTL, 0);
        hal.mmio_write32(mmio_base + REG_RDBAH, (rx_ring.phys >> 32) as u32);
        hal.mmio_write32(mmio_base + REG_RDBAL, rx_ring.phys as u32);
        hal.mmio_write32(mmio_base + REG_RDLEN, rx_ring_bytes as u32);
        hal.mmio_write32(mmio_base + REG_RDH, 0);
        hal.mmio_write32(mmio_base + REG_RDT, 0);

        let rxdctl_orig = hal.mmio_read32(mmio_base + REG_RXDCTL);
        let rxdctl_base = if rxdctl_orig == 0xFFFF_FFFF {
            0
        } else {
            rxdctl_orig & !(0x3F | (0x3F << 8) | (0x3F << 16))
        };
        let rxdctl = rxdctl_base | 16 | (4 << 8) | (1 << 16) | RXDCTL_ENABLE;
        hal.mmio_write32(mmio_base + REG_RXDCTL, rxdctl);
        hal.mmio_write32(mmio_base + REG_RXDCTL + 0x100, 0);

        for _ in 0..1_000 {
            if (hal.mmio_read32(mmio_base + REG_RXDCTL) & RXDCTL_ENABLE) != 0 {
                break;
            }
        }

        let rctl = RCTL_EN | RCTL_UPE | RCTL_MPE | RCTL_BAM | RCTL_SZ_2048 | RCTL_SECRC;
        hal.mmio_write32(mmio_base + REG_RCTL, rctl);
        fence(Ordering::SeqCst);
        Self::safe_write_tail(&hal, mmio_base, REG_RDT, (NUM_RX_DESC - 1) as u32);

        // 10. Initialize TX ring
        let tx_descs_ptr = tx_ring.virt as *mut TxDesc;
        for i in 0..NUM_TX_DESC {
            let desc = TxDesc {
                buffer_addr: tx_bufs.phys + (i * BUFFER_SIZE) as u64,
                length: 0,
                cso: 0,
                cmd: 0,
                status: 1, // DD=1: available
                css: 0,
                special: 0,
            };
            unsafe {
                ptr::write_volatile(tx_descs_ptr.add(i), desc);
            }
        }
        fence(Ordering::SeqCst);

        hal.mmio_write32(mmio_base + REG_TDBAH, (tx_ring.phys >> 32) as u32);
        hal.mmio_write32(mmio_base + REG_TDBAL, tx_ring.phys as u32);
        hal.mmio_write32(mmio_base + REG_TDLEN, tx_ring_bytes as u32);
        hal.mmio_write32(mmio_base + REG_TDH, 0);
        Self::safe_write_tail(&hal, mmio_base, REG_TDT, 0);

        let tipg_val = if tipg_init != 0 && tipg_init != 0xFFFF_FFFF {
            tipg_init
        } else {
            10 | (8 << 10) | (6 << 20)
        };
        hal.mmio_write32(mmio_base + REG_TIPG, tipg_val);

        let txdctl_orig = hal.mmio_read32(mmio_base + REG_TXDCTL);
        let txdctl_base = if txdctl_orig == 0xFFFF_FFFF {
            0
        } else {
            txdctl_orig & !(0x3F | (0x3F << 8) | (0x3F << 16))
        };
        let txdctl = txdctl_base | TXDCTL_DMA_BURST | TXDCTL_ENABLE;
        hal.mmio_write32(mmio_base + REG_TXDCTL, txdctl);
        hal.mmio_write32(mmio_base + REG_TXDCTL + 0x100, txdctl);

        for _ in 0..1_000 {
            if (hal.mmio_read32(mmio_base + REG_TXDCTL) & TXDCTL_ENABLE) != 0 {
                break;
            }
        }

        let tctl = if tctl_init != 0 && tctl_init != 0xFFFF_FFFF {
            (tctl_init & !TCTL_MULR) | TCTL_EN | TCTL_PSP
        } else {
            TCTL_EN | TCTL_PSP | (0x0F << TCTL_CT_SHIFT) | (0x40 << TCTL_COLD_SHIFT)
        };
        hal.mmio_write32(mmio_base + REG_TCTL, tctl);

        let hw_tdh = (hal.mmio_read32(mmio_base + REG_TDH) as usize) % NUM_TX_DESC;
        if hw_tdh != 0 {
            Self::safe_write_tail(&hal, mmio_base, REG_TDT, hw_tdh as u32);
        }

        Ok(Self {
            pci,
            mmio_base,
            hal,
            mac,
            rx_ring,
            rx_bufs,
            rx_cur: 0,
            tx_ring,
            tx_bufs,
            tx_cur: hw_tdh,
            tx_clean: hw_tdh,
        })
    }

    pub fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    pub fn is_link_up(&self) -> bool {
        (self.mmio_read(REG_STATUS) & (1 << 1)) != 0
    }

    fn clean_tx(&mut self) {
        let tx_descs = self.tx_ring.virt as *mut TxDesc;
        while self.tx_clean != self.tx_cur {
            let status = unsafe { ptr::read_volatile(&(*tx_descs.add(self.tx_clean)).status) };
            if (status & 1) != 0 {
                self.tx_clean = (self.tx_clean + 1) % NUM_TX_DESC;
            } else {
                break;
            }
        }

        if self.tx_clean != self.tx_cur {
            let tdh = (self.mmio_read(REG_TDH) as usize) % NUM_TX_DESC;
            let advanced = if self.tx_cur >= self.tx_clean {
                tdh > self.tx_clean && tdh <= self.tx_cur
            } else {
                tdh > self.tx_clean || tdh <= self.tx_cur
            };
            if advanced {
                while self.tx_clean != tdh {
                    unsafe {
                        ptr::write_volatile(&mut (*tx_descs.add(self.tx_clean)).status, 1);
                    }
                    self.tx_clean = (self.tx_clean + 1) % NUM_TX_DESC;
                }
            }
        }
    }

    /// Reclaims completed TX descriptors (via `TxDesc.status & 1` or hardware `REG_TDH` fallback)
    /// and returns `true` if at least one TX descriptor slot is free.
    pub fn can_transmit(&mut self) -> bool {
        self.clean_tx();
        let tx_descs = self.tx_ring.virt as *const TxDesc;
        let cur_status = unsafe { ptr::read_volatile(&(*tx_descs.add(self.tx_cur)).status) };
        (self.tx_cur + 1) % NUM_TX_DESC != self.tx_clean && (cur_status & 1) != 0
    }

    /// Submits `packet` to the TX ring if `can_transmit()` is true. Never spin-waits or resets the TX ring.
    pub fn transmit(&mut self, packet: &[u8]) -> Result<()> {
        if packet.len() > BUFFER_SIZE {
            return Err(Error::msg("Packet exceeds e1000 BUFFER_SIZE"));
        }
        if !self.can_transmit() {
            return Err(Error::msg("E1000 TX ring is full"));
        }

        let idx = self.tx_cur;
        let buf_phys = self.tx_bufs.phys + (idx * BUFFER_SIZE) as u64;
        let buf_ptr = unsafe { self.tx_bufs.virt.add(idx * BUFFER_SIZE) };
        let desc_ptr = unsafe { (self.tx_ring.virt as *mut TxDesc).add(idx) };

        unsafe {
            ptr::copy_nonoverlapping(packet.as_ptr(), buf_ptr, packet.len());
            ptr::write_volatile(
                desc_ptr,
                TxDesc {
                    buffer_addr: buf_phys,
                    length: packet.len() as u16,
                    cso: 0,
                    cmd: CMD_EOP | CMD_IFCS | CMD_RS,
                    status: 0,
                    css: 0,
                    special: 0,
                },
            );
        }
        fence(Ordering::SeqCst);

        self.tx_cur = (idx + 1) % NUM_TX_DESC;
        Self::safe_write_tail(&self.hal, self.mmio_base, REG_TDT, self.tx_cur as u32);
        Ok(())
    }

    /// Returns `true` if the current RX descriptor has `DD` (`status & 1`) set.
    pub fn has_packets(&self) -> bool {
        let rx_descs = self.rx_ring.virt as *const RxDesc;
        let status = unsafe { ptr::read_volatile(&(*rx_descs.add(self.rx_cur)).status) };
        (status & 1) != 0
    }

    /// Receives the next packet into `buf`, replenishes the RX descriptor (`status = 0`),
    /// updates `REG_RDT` to the replenished descriptor index (so `RDT` always trails `rx_cur`),
    /// and advances `rx_cur`. Returns `None` if no valid packet is ready.
    pub fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        const RX_FATAL_ERRORS: u8 = 0x83;
        let rx_descs = self.rx_ring.virt as *mut RxDesc;

        for _ in 0..NUM_RX_DESC {
            let idx = self.rx_cur;
            let desc_ptr = unsafe { rx_descs.add(idx) };
            let desc = unsafe { ptr::read_volatile(desc_ptr) };
            if (desc.status & 1) == 0 {
                return None;
            }

            fence(Ordering::SeqCst);
            let buf_phys = self.rx_bufs.phys + (idx * BUFFER_SIZE) as u64;

            if (desc.errors & RX_FATAL_ERRORS) != 0 || (desc.status & 2) == 0 {
                // Recycle bad or non-EOP descriptor and continue scanning ring
                unsafe {
                    ptr::write_volatile(
                        desc_ptr,
                        RxDesc {
                            buffer_addr: buf_phys,
                            length: 0,
                            checksum: 0,
                            status: 0,
                            errors: 0,
                            special: 0,
                        },
                    );
                }
                fence(Ordering::SeqCst);
                self.rx_cur = (idx + 1) % NUM_RX_DESC;
                Self::safe_write_tail(&self.hal, self.mmio_base, REG_RDT, idx as u32);
                continue;
            }

            let copy_len = (desc.length as usize).min(buf.len());
            let pkt_ptr = unsafe { self.rx_bufs.virt.add(idx * BUFFER_SIZE) };
            unsafe {
                ptr::copy_nonoverlapping(pkt_ptr, buf.as_mut_ptr(), copy_len);
                ptr::write_volatile(
                    desc_ptr,
                    RxDesc {
                        buffer_addr: buf_phys,
                        length: 0,
                        checksum: 0,
                        status: 0,
                        errors: 0,
                        special: 0,
                    },
                );
            }
            fence(Ordering::SeqCst);

            self.rx_cur = (idx + 1) % NUM_RX_DESC;
            Self::safe_write_tail(&self.hal, self.mmio_base, REG_RDT, idx as u32);
            return Some(copy_len);
        }
        None
    }

    pub fn read_stats(&self) -> E1000Stats {
        E1000Stats {
            gprc: self.mmio_read(REG_GPRC),
            gptc: self.mmio_read(REG_GPTC),
            mpc: self.mmio_read(REG_MPC),
            rnbc: self.mmio_read(REG_RNBC),
            crcerrs: self.mmio_read(REG_CRCERRS),
            rdh: self.mmio_read(REG_RDH),
            rdt: self.mmio_read(REG_RDT),
            tdh: self.mmio_read(REG_TDH),
            tdt: self.mmio_read(REG_TDT),
            status: self.mmio_read(REG_STATUS),
            tctl: self.mmio_read(REG_TCTL),
            txdctl: self.mmio_read(REG_TXDCTL),
            rctl: self.mmio_read(REG_RCTL),
            rxdctl: self.mmio_read(REG_RXDCTL),
        }
    }
}

impl<H: Hal> Drop for E1000Device<H> {
    fn drop(&mut self) {
        self.mmio_write(REG_RCTL, 0);
        self.mmio_write(REG_TCTL, 0);
        self.mmio_write(REG_IMC, 0xFFFF_FFFF);
        let _ = self.mmio_read(REG_ICR);

        self.hal.free_dma(self.rx_ring);
        self.hal.free_dma(self.rx_bufs);
        self.hal.free_dma(self.tx_ring);
        self.hal.free_dma(self.tx_bufs);
    }
}

/// Returns `true` if `pci` is a supported Intel e1000 / e1000e Ethernet controller
/// (excluding Intel Wi-Fi cards and Intel `igc` I225/I226 2.5GbE controllers).
pub fn probe(pci: &PciDevice) -> bool {
    if pci.vendor_id != 0x8086 || pci.class_code != 0x02 || pci.subclass != 0x00 {
        return false;
    }
    if crate::net::igc::probe(pci) {
        return false;
    }
    if pci.device_id == 0x24F3
        || pci.device_id == 0x24F4
        || (pci.device_id & 0xFF00) == 0x2400
        || (pci.device_id & 0xFF00) == 0x2700
    {
        return false;
    }
    true
}
