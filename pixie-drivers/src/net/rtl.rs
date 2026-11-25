use core::ptr;
use core::sync::atomic::{Ordering, fence};

use crate::error::{Error, Result};
use crate::hal::{DmaBuffer, Hal, PAGE_SIZE, alloc_dma_buffers};
use crate::net::resolve_mac;
use crate::pci::PciDevice;

pub const NUM_RX_DESC: usize = 256;
pub const NUM_TX_DESC: usize = 128;
pub const BUFFER_SIZE: usize = 2048;

const REG_MAC0: u64 = 0x00;
const REG_IDR4: u64 = 0x04;
const REG_TX_DESC_LOW: u64 = 0x20;
const REG_TX_DESC_HIGH: u64 = 0x24;
const REG_CR: u64 = 0x37;
const REG_TPPOLL_8169: u64 = 0x38;
const REG_IMR_8125: u64 = 0x38;
const REG_IMR_8169: u64 = 0x3C;
const REG_ISR_8125: u64 = 0x3C;
const REG_ISR_8169: u64 = 0x3E;
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

#[inline]
unsafe fn write_rtl_desc_ordered(desc_ptr: *mut RtlDesc, opts1: u32, opts2: u32, buf_phys: u64) {
    // Write all descriptor payload fields BEFORE writing opts1 (which sets DESC_OWN at bit 31)
    // so hardware polling the ring never observes DESC_OWN == 1 with stale address/length fields.
    unsafe {
        ptr::write_volatile(&raw mut (*desc_ptr).opts2, opts2);
        ptr::write_volatile(&raw mut (*desc_ptr).buf_addr_low, buf_phys as u32);
        ptr::write_volatile(&raw mut (*desc_ptr).buf_addr_high, (buf_phys >> 32) as u32);
        fence(Ordering::Release);
        ptr::write_volatile(&raw mut (*desc_ptr).opts1, opts1);
    }
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

    fn mask_and_clear_interrupts(hal: &H, reg_base: u64, is_io: bool, variant: RtlVariant) {
        match variant {
            RtlVariant::Rtl8169 => {
                Self::reg_write16(hal, reg_base, is_io, REG_IMR_8169, 0x0000);
                Self::reg_write16(hal, reg_base, is_io, REG_ISR_8169, 0xFFFF);
            }
            RtlVariant::Rtl8125 => {
                Self::reg_write32(hal, reg_base, is_io, REG_IMR_8125, 0x0000_0000);
                Self::reg_write32(hal, reg_base, is_io, REG_ISR_8125, 0xFFFF_FFFF);
            }
        }
    }

    fn wait_reset_done(hal: &H, reg_base: u64, is_io: bool, timeout_us: i64) -> Option<bool> {
        let start = hal.micros();
        let mut last = start;
        let mut stalled = 0usize;
        loop {
            let cr = Self::reg_read8(hal, reg_base, is_io, REG_CR);
            if cr == 0xFF {
                return None;
            }
            if (cr & CR_RST) == 0 {
                return Some(true);
            }
            let now = hal.micros();
            if now.saturating_sub(start) >= timeout_us {
                return Some(false);
            }
            if now == last {
                stalled += 1;
                if stalled >= 20_000 {
                    return Some(false);
                }
            } else {
                last = now;
                stalled = 0;
            }
            core::hint::spin_loop();
        }
    }

    /// Initializes a Realtek RTL8111/8168/8169 (1GbE) or RTL8125/8126 (2.5GbE) controller:
    /// 1. Reads MAC from `MAC0..5` (falling back to `fallback_mac` or locally-administered default).
    /// 2. Issues software reset (`CR = CR_RST`, waiting for `CR_RST == 0`).
    /// 3. Unlocks config registers (`9346CR = 0xC0`), restores MAC registers via 32-bit `IDR0`/`IDR4` writes.
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
        let variant = probe(&pci).unwrap_or(if matches!(pci.device_id, 0x8125 | 0x8126 | 0x3000) {
            RtlVariant::Rtl8125
        } else {
            RtlVariant::Rtl8169
        });

        pci.enable_bus_mastering(&hal);
        if !is_io {
            hal.map_mmio(reg_base, 0x4000);
        }

        if Self::reg_read8(&hal, reg_base, is_io, REG_CR) == 0xFF {
            return Err(Error::msg("Realtek NIC not present (CR=0xFF)"));
        }

        // 1. Read MAC from MAC0..5
        let mut hw_mac = [0u8; 6];
        for (i, byte) in hw_mac.iter_mut().enumerate() {
            *byte = Self::reg_read8(&hal, reg_base, is_io, REG_MAC0 + i as u64);
        }
        let mac = resolve_mac(hw_mac, fallback_mac);

        // 2. Software reset
        Self::reg_write8(&hal, reg_base, is_io, REG_CR, CR_RST);
        match Self::wait_reset_done(&hal, reg_base, is_io, 50_000) {
            Some(true) => {}
            Some(false) => return Err(Error::msg("Realtek software reset timed out")),
            None => return Err(Error::msg("Realtek NIC detached during reset")),
        }

        // 3. Unlock config registers (9346CR = 0xC0) and restore MAC via 32-bit IDR0/IDR4 writes
        Self::reg_write8(&hal, reg_base, is_io, REG_9346CR, 0xC0);
        let idr0 = u32::from_le_bytes([mac[0], mac[1], mac[2], mac[3]]);
        let idr4 = u32::from_le_bytes([mac[4], mac[5], 0, 0]);
        Self::reg_write32(&hal, reg_base, is_io, REG_IDR4, idr4);
        Self::reg_write32(&hal, reg_base, is_io, REG_MAC0, idr0);

        // 4. Configure RMS = 2048, MTPS = 0x3B, and CPCR (RxChkSum | MulRW, NO PCIDAC)
        Self::reg_write16(&hal, reg_base, is_io, REG_RMS, BUFFER_SIZE as u16);
        Self::reg_write8(&hal, reg_base, is_io, REG_MTPS, 0x3B);
        Self::reg_write16(&hal, reg_base, is_io, REG_CPCR, (1 << 5) | (1 << 3));

        // 5. Allocate and program RX and TX descriptor rings + buffers
        let rx_ring_pages = (NUM_RX_DESC * core::mem::size_of::<RtlDesc>()).div_ceil(PAGE_SIZE);
        let rx_buf_pages = (NUM_RX_DESC * BUFFER_SIZE).div_ceil(PAGE_SIZE);
        let tx_ring_pages = (NUM_TX_DESC * core::mem::size_of::<RtlDesc>()).div_ceil(PAGE_SIZE);
        let tx_buf_pages = (NUM_TX_DESC * BUFFER_SIZE).div_ceil(PAGE_SIZE);

        let [rx_ring, rx_bufs, tx_ring, tx_bufs] = alloc_dma_buffers(
            &hal,
            [rx_ring_pages, rx_buf_pages, tx_ring_pages, tx_buf_pages],
            "Out of DMA memory for Realtek",
        )?;

        let rx_descs_ptr = rx_ring.virt as *mut RtlDesc;
        for i in 0..NUM_RX_DESC {
            let mut flags = DESC_OWN | (BUFFER_SIZE as u32 & 0x3FFF);
            if i == NUM_RX_DESC - 1 {
                flags |= DESC_EOR;
            }
            let buf_phys = rx_bufs.phys + (i * BUFFER_SIZE) as u64;
            unsafe {
                write_rtl_desc_ordered(rx_descs_ptr.add(i), flags, 0, buf_phys);
            }
        }

        let tx_descs_ptr = tx_ring.virt as *mut RtlDesc;
        for i in 0..NUM_TX_DESC {
            let flags = if i == NUM_TX_DESC - 1 { DESC_EOR } else { 0 };
            let buf_phys = tx_bufs.phys + (i * BUFFER_SIZE) as u64;
            unsafe {
                write_rtl_desc_ordered(tx_descs_ptr.add(i), flags, 0, buf_phys);
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
        Self::mask_and_clear_interrupts(&hal, reg_base, is_io, variant);

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
        if !self.is_io && self.hal.mmio_read32(self.reg_base + (REG_PHY_STATUS & !3)) == 0xFFFF_FFFF
        {
            return false;
        }
        let phy = Self::reg_read8(&self.hal, self.reg_base, self.is_io, REG_PHY_STATUS);
        phy != 0xFF && (phy & 0x02) != 0
    }

    pub fn can_transmit(&mut self) -> bool {
        let tx_descs = self.tx_ring.virt as *const RtlDesc;
        let opts1 = unsafe { ptr::read_volatile(&raw const (*tx_descs.add(self.tx_cur)).opts1) };
        (opts1 & DESC_OWN) == 0
    }

    /// Transmits `packet`, preserving `DESC_EOR` on the final descriptor and ringing `0x38` (`0x40`) on Rtl8169
    /// or `0x90` (`0x01`) on Rtl8125.
    pub fn transmit(&mut self, packet: &[u8]) -> Result<()> {
        if packet.is_empty() || packet.len() > BUFFER_SIZE {
            return Err(Error::msg("Invalid Realtek packet length"));
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
            write_rtl_desc_ordered(desc_ptr, flags, 0, buf_phys);
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
        let opts1 = unsafe { ptr::read_volatile(&raw const (*rx_descs.add(self.rx_cur)).opts1) };
        (opts1 & DESC_OWN) == 0
    }

    /// Receives the next frame into `buf`, stripping the 4-byte Ethernet FCS (`raw_len.saturating_sub(4)`),
    /// re-arming the descriptor with `DESC_OWN` (and `DESC_EOR` on `NUM_RX_DESC - 1`), and advancing `rx_cur`.
    pub fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        let rx_descs = self.rx_ring.virt as *mut RtlDesc;
        for _ in 0..NUM_RX_DESC {
            let idx = self.rx_cur;
            let desc_ptr = unsafe { rx_descs.add(idx) };
            let opts1 = unsafe { ptr::read_volatile(&raw const (*desc_ptr).opts1) };
            if (opts1 & DESC_OWN) != 0 {
                return None;
            }

            fence(Ordering::Acquire);
            let buf_phys = self.rx_bufs.phys + (idx * BUFFER_SIZE) as u64;
            let mut rearm_flags = DESC_OWN | (BUFFER_SIZE as u32 & 0x3FFF);
            if idx == NUM_RX_DESC - 1 {
                rearm_flags |= DESC_EOR;
            }

            let is_complete = (opts1 & (DESC_FS | DESC_LS)) == (DESC_FS | DESC_LS);
            let has_error = (opts1 & DESC_RX_RES) != 0;
            let raw_len = (opts1 & 0x3FFF) as usize;
            let frame_len = raw_len.saturating_sub(4);

            if !is_complete || has_error || raw_len > BUFFER_SIZE || frame_len == 0 {
                unsafe {
                    write_rtl_desc_ordered(desc_ptr, rearm_flags, 0, buf_phys);
                }
                fence(Ordering::SeqCst);
                self.rx_cur = (idx + 1) % NUM_RX_DESC;
                continue;
            }

            let copy_len = frame_len.min(BUFFER_SIZE).min(buf.len());
            let pkt_ptr = unsafe { self.rx_bufs.virt.add(idx * BUFFER_SIZE) };
            unsafe {
                ptr::copy_nonoverlapping(pkt_ptr, buf.as_mut_ptr(), copy_len);
                write_rtl_desc_ordered(desc_ptr, rearm_flags, 0, buf_phys);
            }
            fence(Ordering::SeqCst);

            self.rx_cur = (idx + 1) % NUM_RX_DESC;
            return Some(copy_len);
        }
        None
    }
}

impl<H: Hal> Drop for RtlDevice<H> {
    fn drop(&mut self) {
        Self::reg_write8(&self.hal, self.reg_base, self.is_io, REG_CR, 0);
        Self::mask_and_clear_interrupts(&self.hal, self.reg_base, self.is_io, self.variant);
        let cmd = self.pci.read_u16(&self.hal, 0x04);
        if cmd != 0xFFFF && (cmd & 0x0004) != 0 {
            self.pci.write_u16(&self.hal, 0x04, cmd & !0x0004);
        }
        Self::reg_write8(&self.hal, self.reg_base, self.is_io, REG_CR, CR_RST);
        let stopped =
            Self::wait_reset_done(&self.hal, self.reg_base, self.is_io, 50_000).unwrap_or(true);
        fence(Ordering::SeqCst);

        if stopped {
            self.hal.free_dma(self.rx_ring);
            self.hal.free_dma(self.rx_bufs);
            self.hal.free_dma(self.tx_ring);
            self.hal.free_dma(self.tx_bufs);
        }
    }
}

/// Returns `Some(RtlVariant)` if `pci` is a supported Realtek wired Ethernet controller.
pub fn probe(pci: &PciDevice) -> Option<RtlVariant> {
    if pci.class_code != 0x02 || pci.subclass != 0x00 {
        return None;
    }
    if pci.vendor_id == 0x10EC {
        if matches!(pci.device_id, 0x8125 | 0x8126 | 0x3000) {
            return Some(RtlVariant::Rtl8125);
        }
        if pci.device_id == 0x8139
            || (0x8170..=0x8199).contains(&pci.device_id)
            || matches!(pci.device_id & 0xF000, 0xB000 | 0xC000 | 0xD000)
            || matches!(pci.device_id, 0x8812 | 0x8821 | 0x8822 | 0x8852)
        {
            return None;
        }
        Some(RtlVariant::Rtl8169)
    } else if (pci.vendor_id == 0x1186 && matches!(pci.device_id, 0x4300 | 0x4302))
        || (pci.vendor_id == 0x1259 && pci.device_id == 0xC107)
        || (pci.vendor_id == 0x16EC && pci.device_id == 0x0116)
        || (pci.vendor_id == 0x1737 && pci.device_id == 0x1032)
    {
        Some(RtlVariant::Rtl8169)
    } else {
        None
    }
}
