use core::ptr;
use core::sync::atomic::{Ordering, fence};

use crate::error::{Error, Result};
use crate::hal::{DmaBuffer, Hal, PAGE_SIZE};
use crate::pci::PciDevice;

pub const NUM_RX_DESC: usize = 256;
pub const NUM_TX_DESC: usize = 128;
pub const BUFFER_SIZE: usize = 2048;

const REG_MAC0: u64 = 0x00;
const REG_TX_DESC_LOW: u64 = 0x20;
const REG_TX_DESC_HIGH: u64 = 0x24;
const REG_CR: u64 = 0x37;
const REG_TPPOLL_8169: u64 = 0x38;
const REG_IMR: u64 = 0x3C;
const REG_ISR: u64 = 0x3E;
const REG_TCR: u64 = 0x40;
const REG_RCR: u64 = 0x44;
const REG_9346CR: u64 = 0x50;
const REG_PHY_STATUS: u64 = 0x6C;
const REG_TPPOLL_8125: u64 = 0x90;
const REG_RMS: u64 = 0xDA;
const REG_CPCR: u64 = 0xE0;
const REG_RX_DESC_LOW: u64 = 0xE4;
const REG_RX_DESC_HIGH: u64 = 0xE8;
const REG_MTPS: u64 = 0xEC;

const CR_RST: u8 = 1 << 4;
const CR_RE: u8 = 1 << 3;
const CR_TE: u8 = 1 << 2;

const DESC_OWN: u32 = 1 << 31;
const DESC_EOR: u32 = 1 << 30;
const DESC_FS: u32 = 1 << 29;
const DESC_LS: u32 = 1 << 28;
const DESC_RX_RES: u32 = 1 << 21;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtlVariant {
    /// 1GbE / FastE Realtek family (RTL8111 / RTL8168 / RTL8169 / RTL8101E)
    Rtl8169,
    /// 2.5GbE Realtek family (RTL8125 / RTL8126)
    Rtl8125,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RtlDesc {
    pub opts1: u32,
    pub opts2: u32,
    pub buf_addr_low: u32,
    pub buf_addr_high: u32,
}

pub struct RtlDevice<H: Hal> {
    pub pci: PciDevice,
    pub reg_base: u64,
    pub is_io: bool,
    pub variant: RtlVariant,
    pub hal: H,
    mac: [u8; 6],
    rx_ring: DmaBuffer,
    rx_bufs: DmaBuffer,
    rx_cur: usize,
    tx_ring: DmaBuffer,
    tx_bufs: DmaBuffer,
    tx_cur: usize,
}

impl<H: Hal> RtlDevice<H> {
    fn reg_read8(hal: &H, base: u64, is_io: bool, offset: u64) -> u8 {
        if is_io {
            hal.io_read8((base + offset) as u16)
        } else {
            hal.mmio_read8(base + offset)
        }
    }

    fn reg_write8(hal: &H, base: u64, is_io: bool, offset: u64, val: u8) {
        if is_io {
            hal.io_write8((base + offset) as u16, val);
        } else {
            hal.mmio_write8(base + offset, val);
        }
    }

    fn reg_write16(hal: &H, base: u64, is_io: bool, offset: u64, val: u16) {
        if is_io {
            hal.io_write16((base + offset) as u16, val);
        } else {
            hal.mmio_write16(base + offset, val);
        }
    }

    fn reg_write32(hal: &H, base: u64, is_io: bool, offset: u64, val: u32) {
        if is_io {
            hal.io_write32((base + offset) as u16, val);
        } else {
            hal.mmio_write32(base + offset, val);
        }
    }

    /// Initializes a Realtek RTL8111/8168/8169 (1GbE) or RTL8125/8126 (2.5GbE) controller:
    /// 1. Reads MAC from `MAC0..5` (falling back to `fallback_mac` if all-0s or all-1s).
    /// 2. Issues software reset (`CR = CR_RST`, waiting for `CR_RST == 0`).
    /// 3. Unlocks config registers (`9346CR = 0xC0`), restores MAC registers.
    /// 4. Configures `RMS = 2048` and `CPCR` (`RxChkSum | MulRW`, avoiding legacy PCI `PCIDAC` on PCIe chips).
    /// 5. Allocates and programs 256-byte-aligned RX (`RDSAR`) and TX (`TNPDS`) descriptor rings with `DESC_EOR` on the final descriptor.
    /// 6. Enables `CR = CR_TE | CR_RE` (`0x37`) **BEFORE** writing `TCR` (`0x40`) and `RCR` (`0x44`).
    /// 7. Programs `TCR` and `RCR` (`AAP | APM | AM | AB` + max DMA burst), then locks `9346CR = 0x00` and masks interrupts.
    pub fn new(
        pci: PciDevice,
        reg_base: u64,
        is_io: bool,
        fallback_mac: Option<[u8; 6]>,
        hal: H,
    ) -> Result<Self> {
        if reg_base == 0 {
            return Err(Error::msg("Realtek register base is 0"));
        }
        let variant = probe(&pci).unwrap_or(
            if pci.device_id == 0x8125 || pci.device_id == 0x8126 || pci.device_id == 0x3000 {
                RtlVariant::Rtl8125
            } else {
                RtlVariant::Rtl8169
            },
        );

        pci.enable_bus_mastering(&hal);
        if !is_io {
            hal.map_mmio(reg_base, 0x4000);
        }

        // 1. Read MAC from MAC0..5
        let mut mac = [0u8; 6];
        for (i, byte) in mac.iter_mut().enumerate() {
            *byte = Self::reg_read8(&hal, reg_base, is_io, REG_MAC0 + i as u64);
        }
        let is_valid_mac = |m: &[u8; 6]| *m != [0; 6] && *m != [0xFF; 6] && (m[0] & 1) == 0;
        if !is_valid_mac(&mac)
            && let Some(fb) = fallback_mac
        {
            mac = fb;
        }

        // 2. Software reset
        Self::reg_write8(&hal, reg_base, is_io, REG_CR, CR_RST);
        let start = hal.micros();
        for _ in 0..10_000 {
            if (Self::reg_read8(&hal, reg_base, is_io, REG_CR) & CR_RST) == 0 {
                break;
            }
            if hal.micros().saturating_sub(start) > 50_000 {
                break;
            }
        }

        // 3. Unlock config registers (9346CR = 0xC0) and restore MAC
        Self::reg_write8(&hal, reg_base, is_io, REG_9346CR, 0xC0);
        for (i, &byte) in mac.iter().enumerate() {
            Self::reg_write8(&hal, reg_base, is_io, REG_MAC0 + i as u64, byte);
        }

        // 4. Configure RMS = 2048, MTPS = 0x3B, and CPCR (RxChkSum | MulRW, NO PCIDAC)
        Self::reg_write16(&hal, reg_base, is_io, REG_RMS, BUFFER_SIZE as u16);
        Self::reg_write8(&hal, reg_base, is_io, REG_MTPS, 0x3B);
        Self::reg_write16(&hal, reg_base, is_io, REG_CPCR, (1 << 5) | (1 << 3));

        // 5. Allocate and program RX and TX descriptor rings + buffers
        let rx_ring_pages = (NUM_RX_DESC * core::mem::size_of::<RtlDesc>()).div_ceil(PAGE_SIZE);
        let rx_buf_pages = (NUM_RX_DESC * BUFFER_SIZE).div_ceil(PAGE_SIZE);
        let tx_ring_pages = (NUM_TX_DESC * core::mem::size_of::<RtlDesc>()).div_ceil(PAGE_SIZE);
        let tx_buf_pages = (NUM_TX_DESC * BUFFER_SIZE).div_ceil(PAGE_SIZE);

        let rx_ring = hal
            .alloc_dma(rx_ring_pages)
            .ok_or_else(|| Error::msg("Out of DMA memory for Realtek RX ring"))?;
        let Some(rx_bufs) = hal.alloc_dma(rx_buf_pages) else {
            hal.free_dma(rx_ring);
            return Err(Error::msg("Out of DMA memory for Realtek RX buffers"));
        };
        let Some(tx_ring) = hal.alloc_dma(tx_ring_pages) else {
            hal.free_dma(rx_ring);
            hal.free_dma(rx_bufs);
            return Err(Error::msg("Out of DMA memory for Realtek TX ring"));
        };
        let Some(tx_bufs) = hal.alloc_dma(tx_buf_pages) else {
            hal.free_dma(rx_ring);
            hal.free_dma(rx_bufs);
            hal.free_dma(tx_ring);
            return Err(Error::msg("Out of DMA memory for Realtek TX buffers"));
        };

        let rx_descs_ptr = rx_ring.virt as *mut RtlDesc;
        for i in 0..NUM_RX_DESC {
            let mut flags = DESC_OWN | (BUFFER_SIZE as u32 & 0x3FFF);
            if i == NUM_RX_DESC - 1 {
                flags |= DESC_EOR;
            }
            let buf_phys = rx_bufs.phys + (i * BUFFER_SIZE) as u64;
            unsafe {
                ptr::write_volatile(
                    rx_descs_ptr.add(i),
                    RtlDesc {
                        opts1: flags,
                        opts2: 0,
                        buf_addr_low: buf_phys as u32,
                        buf_addr_high: (buf_phys >> 32) as u32,
                    },
                );
            }
        }

        let tx_descs_ptr = tx_ring.virt as *mut RtlDesc;
        for i in 0..NUM_TX_DESC {
            let flags = if i == NUM_TX_DESC - 1 { DESC_EOR } else { 0 };
            let buf_phys = tx_bufs.phys + (i * BUFFER_SIZE) as u64;
            unsafe {
                ptr::write_volatile(
                    tx_descs_ptr.add(i),
                    RtlDesc {
                        opts1: flags,
                        opts2: 0,
                        buf_addr_low: buf_phys as u32,
                        buf_addr_high: (buf_phys >> 32) as u32,
                    },
                );
            }
        }
        fence(Ordering::SeqCst);

        Self::reg_write32(
            &hal,
            reg_base,
            is_io,
            REG_RX_DESC_HIGH,
            (rx_ring.phys >> 32) as u32,
        );
        Self::reg_write32(&hal, reg_base, is_io, REG_RX_DESC_LOW, rx_ring.phys as u32);
        Self::reg_write32(
            &hal,
            reg_base,
            is_io,
            REG_TX_DESC_HIGH,
            (tx_ring.phys >> 32) as u32,
        );
        Self::reg_write32(&hal, reg_base, is_io, REG_TX_DESC_LOW, tx_ring.phys as u32);

        // 6. CRITICAL QUIRK FIX: Enable CR = CR_TE | CR_RE BEFORE writing TCR and RCR
        Self::reg_write8(&hal, reg_base, is_io, REG_CR, CR_TE | CR_RE);

        // 7. Program TCR and RCR, then lock 9346CR and mask interrupts
        Self::reg_write32(&hal, reg_base, is_io, REG_TCR, (3 << 24) | (7 << 8));
        Self::reg_write32(&hal, reg_base, is_io, REG_RCR, (7 << 13) | (7 << 8) | 0x0F);
        Self::reg_write8(&hal, reg_base, is_io, REG_9346CR, 0x00);
        Self::reg_write16(&hal, reg_base, is_io, REG_IMR, 0x0000);
        Self::reg_write16(&hal, reg_base, is_io, REG_ISR, 0xFFFF);

        Ok(Self {
            pci,
            reg_base,
            is_io,
            variant,
            hal,
            mac,
            rx_ring,
            rx_bufs,
            rx_cur: 0,
            tx_ring,
            tx_bufs,
            tx_cur: 0,
        })
    }

    pub fn variant(&self) -> RtlVariant {
        self.variant
    }

    pub fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    pub fn is_link_up(&self) -> bool {
        let phy = Self::reg_read8(&self.hal, self.reg_base, self.is_io, REG_PHY_STATUS);
        (phy & 0x02) != 0
    }

    pub fn can_transmit(&mut self) -> bool {
        let tx_descs = self.tx_ring.virt as *const RtlDesc;
        let opts1 = unsafe { ptr::read_volatile(&(*tx_descs.add(self.tx_cur)).opts1) };
        (opts1 & DESC_OWN) == 0
    }

    /// Transmits `packet`, preserving `DESC_EOR` on the final descriptor and ringing `0x38` (`0x40`) on Rtl8169
    /// or `0x90` (`0x01`) on Rtl8125.
    pub fn transmit(&mut self, packet: &[u8]) -> Result<()> {
        if packet.len() > BUFFER_SIZE {
            return Err(Error::msg("Packet exceeds Realtek BUFFER_SIZE"));
        }
        if !self.can_transmit() {
            return Err(Error::msg("Realtek TX ring is full"));
        }

        let idx = self.tx_cur;
        let buf_phys = self.tx_bufs.phys + (idx * BUFFER_SIZE) as u64;
        let buf_ptr = unsafe { self.tx_bufs.virt.add(idx * BUFFER_SIZE) };
        let desc_ptr = unsafe { (self.tx_ring.virt as *mut RtlDesc).add(idx) };

        let mut flags = DESC_OWN | DESC_FS | DESC_LS | (packet.len() as u32 & 0x3FFF);
        if idx == NUM_TX_DESC - 1 {
            flags |= DESC_EOR;
        }

        unsafe {
            ptr::copy_nonoverlapping(packet.as_ptr(), buf_ptr, packet.len());
            ptr::write_volatile(
                desc_ptr,
                RtlDesc {
                    opts1: flags,
                    opts2: 0,
                    buf_addr_low: buf_phys as u32,
                    buf_addr_high: (buf_phys >> 32) as u32,
                },
            );
        }
        fence(Ordering::SeqCst);

        match self.variant {
            RtlVariant::Rtl8169 => {
                Self::reg_write8(&self.hal, self.reg_base, self.is_io, REG_TPPOLL_8169, 0x40);
            }
            RtlVariant::Rtl8125 => {
                Self::reg_write16(
                    &self.hal,
                    self.reg_base,
                    self.is_io,
                    REG_TPPOLL_8125,
                    0x0001,
                );
            }
        }

        self.tx_cur = (idx + 1) % NUM_TX_DESC;
        Ok(())
    }

    pub fn has_packets(&self) -> bool {
        let rx_descs = self.rx_ring.virt as *const RtlDesc;
        let opts1 = unsafe { ptr::read_volatile(&(*rx_descs.add(self.rx_cur)).opts1) };
        (opts1 & DESC_OWN) == 0
    }

    /// Receives the next frame into `buf`, stripping the 4-byte Ethernet FCS (`raw_len.saturating_sub(4)`),
    /// re-arming the descriptor with `DESC_OWN` (and `DESC_EOR` on `NUM_RX_DESC - 1`), and advancing `rx_cur`.
    pub fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        let rx_descs = self.rx_ring.virt as *mut RtlDesc;
        for _ in 0..NUM_RX_DESC {
            let idx = self.rx_cur;
            let desc_ptr = unsafe { rx_descs.add(idx) };
            let desc = unsafe { ptr::read_volatile(desc_ptr) };
            if (desc.opts1 & DESC_OWN) != 0 {
                return None;
            }

            fence(Ordering::SeqCst);
            let buf_phys = self.rx_bufs.phys + (idx * BUFFER_SIZE) as u64;
            let mut rearm_flags = DESC_OWN | (BUFFER_SIZE as u32 & 0x3FFF);
            if idx == NUM_RX_DESC - 1 {
                rearm_flags |= DESC_EOR;
            }

            let is_complete = (desc.opts1 & (DESC_FS | DESC_LS)) == (DESC_FS | DESC_LS);
            let has_error = (desc.opts1 & DESC_RX_RES) != 0;
            let raw_len = (desc.opts1 & 0x3FFF) as usize;
            let frame_len = raw_len.saturating_sub(4);

            if !is_complete || has_error || frame_len == 0 {
                unsafe {
                    ptr::write_volatile(
                        desc_ptr,
                        RtlDesc {
                            opts1: rearm_flags,
                            opts2: 0,
                            buf_addr_low: buf_phys as u32,
                            buf_addr_high: (buf_phys >> 32) as u32,
                        },
                    );
                }
                fence(Ordering::SeqCst);
                self.rx_cur = (idx + 1) % NUM_RX_DESC;
                continue;
            }

            let copy_len = frame_len.min(buf.len());
            let pkt_ptr = unsafe { self.rx_bufs.virt.add(idx * BUFFER_SIZE) };
            unsafe {
                ptr::copy_nonoverlapping(pkt_ptr, buf.as_mut_ptr(), copy_len);
                ptr::write_volatile(
                    desc_ptr,
                    RtlDesc {
                        opts1: rearm_flags,
                        opts2: 0,
                        buf_addr_low: buf_phys as u32,
                        buf_addr_high: (buf_phys >> 32) as u32,
                    },
                );
            }
            fence(Ordering::SeqCst);
            Self::reg_write16(&self.hal, self.reg_base, self.is_io, REG_ISR, 0xFFFF);

            self.rx_cur = (idx + 1) % NUM_RX_DESC;
            return Some(copy_len);
        }
        None
    }
}

impl<H: Hal> Drop for RtlDevice<H> {
    fn drop(&mut self) {
        Self::reg_write8(&self.hal, self.reg_base, self.is_io, REG_CR, 0);
        Self::reg_write16(&self.hal, self.reg_base, self.is_io, REG_IMR, 0);
        Self::reg_write16(&self.hal, self.reg_base, self.is_io, REG_ISR, 0xFFFF);

        self.hal.free_dma(self.rx_ring);
        self.hal.free_dma(self.rx_bufs);
        self.hal.free_dma(self.tx_ring);
        self.hal.free_dma(self.tx_bufs);
    }
}

/// Returns `Some(RtlVariant)` if `pci` is a supported Realtek wired Ethernet controller.
pub fn probe(pci: &PciDevice) -> Option<RtlVariant> {
    if pci.class_code != 0x02 || pci.subclass != 0x00 {
        return None;
    }
    if pci.vendor_id == 0x10EC {
        if pci.device_id == 0x8125 || pci.device_id == 0x8126 || pci.device_id == 0x3000 {
            return Some(RtlVariant::Rtl8125);
        }
        if pci.device_id == 0x8139
            || (pci.device_id >= 0x8170
                && pci.device_id <= 0x8199
                && pci.device_id != 0x8168
                && pci.device_id != 0x8169
                && pci.device_id != 0x8167
                && pci.device_id != 0x8161
                && pci.device_id != 0x8162)
            || (pci.device_id & 0xF000) == 0xC000
            || (pci.device_id & 0xF000) == 0xB000
            || (pci.device_id & 0xF000) == 0xD000
            || pci.device_id == 0x8852
            || pci.device_id == 0x8821
            || pci.device_id == 0x8822
            || pci.device_id == 0x8812
        {
            return None;
        }
        Some(RtlVariant::Rtl8169)
    } else if (pci.vendor_id == 0x1186 && (pci.device_id == 0x4300 || pci.device_id == 0x4302))
        || (pci.vendor_id == 0x1259 && pci.device_id == 0xC107)
        || (pci.vendor_id == 0x16EC && pci.device_id == 0x0116)
        || (pci.vendor_id == 0x1737 && pci.device_id == 0x1032)
    {
        Some(RtlVariant::Rtl8169)
    } else {
        None
    }
}
