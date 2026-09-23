use core::sync::atomic::{Ordering, compiler_fence};

use crate::os::arch::io;
use crate::os::memory;
use crate::os::pci::{Bar, PciDevice};

const NUM_RX_DESC: usize = 64;
const NUM_TX_DESC: usize = 64;
pub const BUFFER_SIZE: usize = 2048;

// Realtek Register Offsets
const REG_MAC0: usize = 0x00;
const REG_TX_DESC_LOW: usize = 0x20;
const REG_TX_DESC_HIGH: usize = 0x24;
const REG_CR: usize = 0x37;
const REG_TPPOLL: usize = 0x38;
const REG_IMR: usize = 0x3C;
const REG_ISR: usize = 0x3E;
const REG_TCR: usize = 0x40;
const REG_RCR: usize = 0x44;
const REG_9346CR: usize = 0x50;
const REG_MSR: usize = 0x58;
const REG_PHY_STATUS: usize = 0x6C;
const REG_RMS: usize = 0xDA;
const REG_CPCR: usize = 0xE0;
const REG_RX_DESC_LOW: usize = 0xE4;
const REG_RX_DESC_HIGH: usize = 0xE8;

// Command Register Bits
const CR_RST: u8 = 1 << 4; // Software Reset
const CR_RE: u8 = 1 << 3; // Receiver Enable
const CR_TE: u8 = 1 << 2; // Transmitter Enable

// Descriptor Option 1 Bits
const DESC_OWN: u32 = 1 << 31; // Owned by NIC
const DESC_EOR: u32 = 1 << 30; // End of Ring
const DESC_FS: u32 = 1 << 29; // First Segment
const DESC_LS: u32 = 1 << 28; // Last Segment

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RtlDesc {
    pub opts1: u32,
    pub opts2: u32,
    pub buf_addr_low: u32,
    pub buf_addr_high: u32,
}

#[derive(Clone, Copy)]
pub enum RegisterAccess {
    Mmio(u64),
    Io(u16),
}

impl RegisterAccess {
    pub unsafe fn read_u8(&self, offset: usize) -> u8 {
        unsafe {
            match *self {
                RegisterAccess::Mmio(base) => {
                    core::ptr::read_volatile((base + offset as u64) as *const u8)
                }
                RegisterAccess::Io(port) => io::inb(port + offset as u16),
            }
        }
    }

    pub unsafe fn write_u8(&self, offset: usize, val: u8) {
        unsafe {
            match *self {
                RegisterAccess::Mmio(base) => {
                    core::ptr::write_volatile((base + offset as u64) as *mut u8, val)
                }
                RegisterAccess::Io(port) => io::outb(port + offset as u16, val),
            }
        }
    }

    #[allow(dead_code)]
    pub unsafe fn read_u16(&self, offset: usize) -> u16 {
        unsafe {
            match *self {
                RegisterAccess::Mmio(base) => {
                    core::ptr::read_volatile((base + offset as u64) as *const u16)
                }
                RegisterAccess::Io(port) => io::inw(port + offset as u16),
            }
        }
    }

    pub unsafe fn write_u16(&self, offset: usize, val: u16) {
        unsafe {
            match *self {
                RegisterAccess::Mmio(base) => {
                    core::ptr::write_volatile((base + offset as u64) as *mut u16, val)
                }
                RegisterAccess::Io(port) => io::outw(port + offset as u16, val),
            }
        }
    }

    #[allow(dead_code)]
    pub unsafe fn read_u32(&self, offset: usize) -> u32 {
        unsafe {
            match *self {
                RegisterAccess::Mmio(base) => {
                    core::ptr::read_volatile((base + offset as u64) as *const u32)
                }
                RegisterAccess::Io(port) => io::inl(port + offset as u16),
            }
        }
    }

    pub unsafe fn write_u32(&self, offset: usize, val: u32) {
        unsafe {
            match *self {
                RegisterAccess::Mmio(base) => {
                    core::ptr::write_volatile((base + offset as u64) as *mut u32, val)
                }
                RegisterAccess::Io(port) => io::outl(port + offset as u16, val),
            }
        }
    }
}

#[allow(dead_code)]
pub struct Rtl8169Device {
    pub pci: PciDevice,
    reg: RegisterAccess,
    mac: [u8; 6],
    rx_descs: &'static mut [RtlDesc],
    rx_bufs: [u64; NUM_RX_DESC],
    rx_cur: usize,
    tx_descs: &'static mut [RtlDesc],
    tx_bufs: [u64; NUM_TX_DESC],
    tx_cur: usize,
}

impl Rtl8169Device {
    pub fn new(pci: PciDevice) -> Result<Self, &'static str> {
        pci.enable_bus_mastering();

        let mut reg_access = None;
        for bar_idx in 0..6 {
            if let Some(bar) = pci.read_bar(bar_idx) {
                match bar {
                    Bar::Memory { base, .. } if base != 0 => {
                        reg_access = Some(RegisterAccess::Mmio(base));
                        break;
                    }
                    Bar::Io { port } if port != 0 && reg_access.is_none() => {
                        reg_access = Some(RegisterAccess::Io(port));
                    }
                    _ => {}
                }
            }
        }

        let reg = reg_access.ok_or("No valid BAR found for RTL8169")?;

        // 1. Read MAC address
        let mut mac = [0u8; 6];
        for (i, byte) in mac.iter_mut().enumerate() {
            *byte = unsafe { reg.read_u8(REG_MAC0 + i) };
        }

        // 2. Reset device
        unsafe {
            reg.write_u8(REG_CR, CR_RST);
        }
        for _ in 0..100_000 {
            let cr = unsafe { reg.read_u8(REG_CR) };
            if (cr & CR_RST) == 0 {
                break;
            }
            core::hint::spin_loop();
        }

        // 3. Unlock configuration registers
        unsafe {
            reg.write_u8(REG_9346CR, 0xC0);
        }

        // 4. Enable Tx and Rx
        unsafe {
            reg.write_u8(REG_CR, CR_TE | CR_RE);
        }

        // 5. Transmit configuration: max DMA burst 1024 bytes, standard IFG
        unsafe {
            reg.write_u32(REG_TCR, (6 << 8) | (3 << 24));
        }

        // 6. Receive configuration: Accept Physical Match, Multicast, Broadcast; unlimited Rx DMA burst
        unsafe {
            reg.write_u32(REG_RCR, 0x0E | (7 << 8) | (7 << 13));
        }

        // 7. Max Rx packet size
        unsafe {
            reg.write_u16(REG_RMS, BUFFER_SIZE as u16);
            reg.write_u16(REG_CPCR, 0);
        }

        // 8. Allocate and initialize Receive Descriptor Ring
        let rx_ring_phys = memory::alloc_page().ok_or("Out of memory for RX ring")?;
        let rx_descs =
            unsafe { core::slice::from_raw_parts_mut(rx_ring_phys as *mut RtlDesc, NUM_RX_DESC) };

        let mut rx_bufs = [0u64; NUM_RX_DESC];
        for i in (0..NUM_RX_DESC).step_by(2) {
            let page = memory::alloc_page().ok_or("Out of memory for RX buffer")?;
            rx_bufs[i] = page;
            rx_bufs[i + 1] = page + (BUFFER_SIZE as u64);
        }

        for i in 0..NUM_RX_DESC {
            let mut flags = DESC_OWN | (BUFFER_SIZE as u32 & 0x3FFF);
            if i == NUM_RX_DESC - 1 {
                flags |= DESC_EOR;
            }
            rx_descs[i] = RtlDesc {
                opts1: flags,
                opts2: 0,
                buf_addr_low: (rx_bufs[i] & 0xFFFF_FFFF) as u32,
                buf_addr_high: ((rx_bufs[i] >> 32) & 0xFFFF_FFFF) as u32,
            };
        }

        unsafe {
            reg.write_u32(REG_RX_DESC_LOW, (rx_ring_phys & 0xFFFF_FFFF) as u32);
            reg.write_u32(
                REG_RX_DESC_HIGH,
                ((rx_ring_phys >> 32) & 0xFFFF_FFFF) as u32,
            );
        }

        // 9. Allocate and initialize Transmit Descriptor Ring
        let tx_ring_phys = memory::alloc_page().ok_or("Out of memory for TX ring")?;
        let tx_descs =
            unsafe { core::slice::from_raw_parts_mut(tx_ring_phys as *mut RtlDesc, NUM_TX_DESC) };

        let mut tx_bufs = [0u64; NUM_TX_DESC];
        for i in (0..NUM_TX_DESC).step_by(2) {
            let page = memory::alloc_page().ok_or("Out of memory for TX buffer")?;
            tx_bufs[i] = page;
            tx_bufs[i + 1] = page + (BUFFER_SIZE as u64);
        }

        for i in 0..NUM_TX_DESC {
            let mut flags = 0;
            if i == NUM_TX_DESC - 1 {
                flags |= DESC_EOR;
            }
            tx_descs[i] = RtlDesc {
                opts1: flags,
                opts2: 0,
                buf_addr_low: (tx_bufs[i] & 0xFFFF_FFFF) as u32,
                buf_addr_high: ((tx_bufs[i] >> 32) & 0xFFFF_FFFF) as u32,
            };
        }

        unsafe {
            reg.write_u32(REG_TX_DESC_LOW, (tx_ring_phys & 0xFFFF_FFFF) as u32);
            reg.write_u32(
                REG_TX_DESC_HIGH,
                ((tx_ring_phys >> 32) & 0xFFFF_FFFF) as u32,
            );
        }

        // 10. Lock configuration registers and mask interrupts
        unsafe {
            reg.write_u8(REG_9346CR, 0x00);
            reg.write_u16(REG_IMR, 0x0000);
            reg.write_u16(REG_ISR, 0xFFFF);
        }

        Ok(Self {
            pci,
            reg,
            mac,
            rx_descs,
            rx_bufs,
            rx_cur: 0,
            tx_descs,
            tx_bufs,
            tx_cur: 0,
        })
    }

    pub fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    pub fn is_link_up(&self) -> bool {
        let phy = unsafe { self.reg.read_u8(REG_PHY_STATUS) };
        let msr = unsafe { self.reg.read_u8(REG_MSR) };
        (phy & 0x02) != 0 || (msr & 0x04) == 0
    }

    pub fn transmit(&mut self, packet: &[u8]) {
        assert!(packet.len() <= BUFFER_SIZE);
        let desc = &mut self.tx_descs[self.tx_cur];

        let mut iters = 0;
        while (unsafe { core::ptr::read_volatile(&desc.opts1) } & DESC_OWN) != 0 {
            core::hint::spin_loop();
            iters += 1;
            if iters > 1_000_000 {
                log::warn!("rtl8169 transmit timeout waiting for descriptor");
                return;
            }
        }

        let buf_ptr = self.tx_bufs[self.tx_cur] as *mut u8;
        unsafe {
            core::ptr::copy_nonoverlapping(packet.as_ptr(), buf_ptr, packet.len());
        }

        let mut flags = DESC_OWN | DESC_FS | DESC_LS | (packet.len() as u32 & 0x3FFF);
        if self.tx_cur == NUM_TX_DESC - 1 {
            flags |= DESC_EOR;
        }

        unsafe {
            core::ptr::write_volatile(&mut desc.opts1, flags);
        }
        compiler_fence(Ordering::SeqCst);

        unsafe {
            self.reg.write_u8(REG_TPPOLL, 0x40); // 0x40: NPQ polling (Normal Priority Queue)
        }

        self.tx_cur = (self.tx_cur + 1) % NUM_TX_DESC;
    }

    pub fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        let desc = &mut self.rx_descs[self.rx_cur];
        let opts1 = unsafe { core::ptr::read_volatile(&desc.opts1) };
        if (opts1 & DESC_OWN) != 0 {
            return None;
        }

        compiler_fence(Ordering::SeqCst);
        let raw_len = (opts1 & 0x3FFF) as usize;
        let pkt_len = if raw_len > 4 { raw_len - 4 } else { raw_len };
        let copy_len = pkt_len.min(buf.len());

        let buf_ptr = self.rx_bufs[self.rx_cur] as *const u8;
        unsafe {
            core::ptr::copy_nonoverlapping(buf_ptr, buf.as_mut_ptr(), copy_len);
        }

        let mut new_flags = DESC_OWN | (BUFFER_SIZE as u32 & 0x3FFF);
        if self.rx_cur == NUM_RX_DESC - 1 {
            new_flags |= DESC_EOR;
        }
        unsafe {
            core::ptr::write_volatile(&mut desc.opts1, new_flags);
        }
        compiler_fence(Ordering::SeqCst);

        self.rx_cur = (self.rx_cur + 1) % NUM_RX_DESC;
        Some(copy_len)
    }

    pub fn has_packets(&self) -> bool {
        let desc = &self.rx_descs[self.rx_cur];
        let opts1 = unsafe { core::ptr::read_volatile(&desc.opts1) };
        (opts1 & DESC_OWN) == 0
    }
}

pub fn probe(pci: &PciDevice) -> bool {
    pci.vendor_id == 0x10EC && pci.class_code == 0x02
}
