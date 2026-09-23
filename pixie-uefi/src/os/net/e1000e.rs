use alloc::vec::Vec;
use core::sync::atomic::{Ordering, compiler_fence};

use crate::os::memory;
use crate::os::pci::{Bar, PciDevice};

const NUM_RX_DESC: usize = 1024;
const NUM_TX_DESC: usize = 256;
pub const BUFFER_SIZE: usize = 2048;

// Intel Register Offsets
const REG_CTRL: usize = 0x0000;
const REG_STATUS: usize = 0x0008;
const REG_EERD: usize = 0x0014;
const REG_ICR: usize = 0x00C0;
const REG_IMC: usize = 0x00D8;
const REG_RCTL: usize = 0x0100;
const REG_TCTL: usize = 0x0400;
const REG_TIPG: usize = 0x0410;
const REG_RDBAL: usize = 0x2800;
const REG_RDBAH: usize = 0x2804;
const REG_RDLEN: usize = 0x2808;
const REG_RDH: usize = 0x2810;
const REG_RDT: usize = 0x2818;
const REG_TDBAL: usize = 0x3800;
const REG_TDBAH: usize = 0x3804;
const REG_TDLEN: usize = 0x3808;
const REG_TDH: usize = 0x3810;
const REG_TDT: usize = 0x3818;
const REG_MTA: usize = 0x5200;
const REG_RAL: usize = 0x5400;
const REG_RAH: usize = 0x5404;

// Control Register Bits
const CTRL_ASDE: u32 = 1 << 5; // Auto-Speed Detection Enable
const CTRL_SLU: u32 = 1 << 6; // Set Link Up
const CTRL_RST: u32 = 1 << 26; // Device Reset

// Receive Control Bits
const RCTL_EN: u32 = 1 << 1; // Receiver Enable
#[allow(dead_code)]
const RCTL_SBP: u32 = 1 << 2; // Store Bad Packets
const RCTL_UPE: u32 = 1 << 3; // Unicast Promiscuous Enable
const RCTL_MPE: u32 = 1 << 4; // Multicast Promiscuous Enable
const RCTL_BAM: u32 = 1 << 15; // Broadcast Accept Mode
const RCTL_SZ_2048: u32 = 0 << 16; // 2048 Byte Buffer Size
const RCTL_SECRC: u32 = 1 << 26; // Strip Ethernet CRC

// Transmit Control Bits
const TCTL_EN: u32 = 1 << 1; // Transmit Enable
const TCTL_PSP: u32 = 1 << 3; // Pad Short Packets
const TCTL_CT_SHIFT: u32 = 4; // Collision Threshold
const TCTL_COLD_SHIFT: u32 = 12; // Collision Distance

// Transmit Command Bits
const CMD_EOP: u8 = 1 << 0; // End of Packet
const CMD_IFCS: u8 = 1 << 1; // Insert FCS/CRC
const CMD_RS: u8 = 1 << 3; // Report Status

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RxDesc {
    pub buffer_addr: u64,
    pub length: u16,
    pub checksum: u16,
    pub status: u8,
    pub errors: u8,
    pub special: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TxDesc {
    pub buffer_addr: u64,
    pub length: u16,
    pub cso: u8,
    pub cmd: u8,
    pub status: u8,
    pub css: u8,
    pub special: u16,
}

#[allow(dead_code)]
pub struct E1000Device {
    pub pci: PciDevice,
    mmio_base: u64,
    mac: [u8; 6],
    rx_descs: &'static mut [RxDesc],
    rx_bufs: Vec<u64>,
    rx_cur: usize,
    tx_descs: &'static mut [TxDesc],
    tx_bufs: Vec<u64>,
    tx_cur: usize,
}

impl E1000Device {
    pub fn new(pci: PciDevice) -> Result<Self, &'static str> {
        pci.enable_bus_mastering();

        let bar0 = pci.read_bar(0).ok_or("Failed to read BAR0")?;
        let mmio_base = match bar0 {
            Bar::Memory { base, .. } if base != 0 => base,
            _ => return Err("BAR0 is not valid MMIO"),
        };

        // 1. Read MAC address before resetting the chip
        let mut mac = [0u8; 6];
        let ral = unsafe { Self::mmio_read(mmio_base, REG_RAL) };
        let rah = unsafe { Self::mmio_read(mmio_base, REG_RAH) };
        if ral != 0 && ral != 0xFFFF_FFFF {
            mac[0] = (ral & 0xFF) as u8;
            mac[1] = ((ral >> 8) & 0xFF) as u8;
            mac[2] = ((ral >> 16) & 0xFF) as u8;
            mac[3] = ((ral >> 24) & 0xFF) as u8;
            mac[4] = (rah & 0xFF) as u8;
            mac[5] = ((rah >> 8) & 0xFF) as u8;
        }

        if mac == [0; 6] {
            // Fall back to reading from EEPROM
            for i in 0..3 {
                let word = Self::read_eeprom(mmio_base, i as u8);
                mac[i * 2] = (word & 0xFF) as u8;
                mac[i * 2 + 1] = ((word >> 8) & 0xFF) as u8;
            }
        }

        if mac == [0; 6] {
            if let Some(info) = *crate::os::boot_info::BOOT_INFO.lock() {
                if let Some(uefi_mac) = info.uefi_mac {
                    mac = uefi_mac;
                }
            }
        }

        // 2. Disable interrupts
        unsafe {
            Self::mmio_write(mmio_base, REG_IMC, 0xFFFF_FFFF);
            let _ = Self::mmio_read(mmio_base, REG_ICR);
        }

        // 3. Reset device
        let ctrl = unsafe { Self::mmio_read(mmio_base, REG_CTRL) };
        unsafe {
            Self::mmio_write(mmio_base, REG_CTRL, ctrl | CTRL_RST);
        }
        let reset_start = crate::os::timer::Timer::micros();
        while (crate::os::timer::Timer::micros() - reset_start) < 50_000 {
            let c = unsafe { Self::mmio_read(mmio_base, REG_CTRL) };
            if (c & CTRL_RST) == 0 {
                break;
            }
            core::hint::spin_loop();
        }

        // 4. Disable interrupts again post-reset
        unsafe {
            Self::mmio_write(mmio_base, REG_IMC, 0xFFFF_FFFF);
            let _ = Self::mmio_read(mmio_base, REG_ICR);
        }

        // 5. Enable auto-speed detection and force link up
        let ctrl2 = unsafe { Self::mmio_read(mmio_base, REG_CTRL) };
        unsafe {
            Self::mmio_write(
                mmio_base,
                REG_CTRL,
                (ctrl2 & !CTRL_RST) | CTRL_ASDE | CTRL_SLU,
            );
        }

        // 6. Zero out Multicast Table Array (MTA)
        for i in 0..128 {
            unsafe {
                Self::mmio_write(mmio_base, REG_MTA + i * 4, 0);
            }
        }

        // 7. Restore MAC in Receive Address register
        let ral_val = (mac[0] as u32)
            | ((mac[1] as u32) << 8)
            | ((mac[2] as u32) << 16)
            | ((mac[3] as u32) << 24);
        let rah_val = (mac[4] as u32) | ((mac[5] as u32) << 8) | (1 << 31);
        unsafe {
            Self::mmio_write(mmio_base, REG_RAL, ral_val);
            Self::mmio_write(mmio_base, REG_RAH, rah_val);
        }

        // 8. Allocate and initialize Receive Descriptor Ring
        let rx_ring_bytes = NUM_RX_DESC * core::mem::size_of::<RxDesc>();
        let rx_ring_pages = rx_ring_bytes.div_ceil(memory::PAGE_SIZE as usize);
        let rx_ring_phys =
            memory::alloc_contiguous(rx_ring_pages).ok_or("Out of memory for RX ring")?;
        let rx_descs =
            unsafe { core::slice::from_raw_parts_mut(rx_ring_phys as *mut RxDesc, NUM_RX_DESC) };

        let mut rx_bufs = alloc::vec![0u64; NUM_RX_DESC];
        for i in (0..NUM_RX_DESC).step_by(2) {
            let page = memory::alloc_page().ok_or("Out of memory for RX buffer")?;
            rx_bufs[i] = page;
            rx_bufs[i + 1] = page + (BUFFER_SIZE as u64);
        }

        for i in 0..NUM_RX_DESC {
            rx_descs[i] = RxDesc {
                buffer_addr: rx_bufs[i],
                length: 0,
                checksum: 0,
                status: 0,
                errors: 0,
                special: 0,
            };
        }

        unsafe {
            Self::mmio_write(
                mmio_base,
                REG_RDBAH,
                ((rx_ring_phys >> 32) & 0xFFFF_FFFF) as u32,
            );
            Self::mmio_write(mmio_base, REG_RDBAL, (rx_ring_phys & 0xFFFF_FFFF) as u32);
            Self::mmio_write(mmio_base, REG_RDLEN, rx_ring_bytes as u32);
            Self::mmio_write(mmio_base, REG_RDH, 0);
            Self::mmio_write(mmio_base, REG_RDT, (NUM_RX_DESC - 1) as u32);
        }

        let rctl = RCTL_EN | RCTL_UPE | RCTL_MPE | RCTL_BAM | RCTL_SZ_2048 | RCTL_SECRC;
        unsafe {
            Self::mmio_write(mmio_base, REG_RCTL, rctl);
        }

        // 9. Allocate and initialize Transmit Descriptor Ring
        let tx_ring_bytes = NUM_TX_DESC * core::mem::size_of::<TxDesc>();
        let tx_ring_pages = tx_ring_bytes.div_ceil(memory::PAGE_SIZE as usize);
        let tx_ring_phys =
            memory::alloc_contiguous(tx_ring_pages).ok_or("Out of memory for TX ring")?;
        let tx_descs =
            unsafe { core::slice::from_raw_parts_mut(tx_ring_phys as *mut TxDesc, NUM_TX_DESC) };

        let mut tx_bufs = alloc::vec![0u64; NUM_TX_DESC];
        for i in (0..NUM_TX_DESC).step_by(2) {
            let page = memory::alloc_page().ok_or("Out of memory for TX buffer")?;
            tx_bufs[i] = page;
            tx_bufs[i + 1] = page + (BUFFER_SIZE as u64);
        }

        for i in 0..NUM_TX_DESC {
            tx_descs[i] = TxDesc {
                buffer_addr: tx_bufs[i],
                length: 0,
                cso: 0,
                cmd: 0,
                status: 1, // DD=1: marked done / available
                css: 0,
                special: 0,
            };
        }

        unsafe {
            Self::mmio_write(
                mmio_base,
                REG_TDBAH,
                ((tx_ring_phys >> 32) & 0xFFFF_FFFF) as u32,
            );
            Self::mmio_write(mmio_base, REG_TDBAL, (tx_ring_phys & 0xFFFF_FFFF) as u32);
            Self::mmio_write(mmio_base, REG_TDLEN, tx_ring_bytes as u32);
            Self::mmio_write(mmio_base, REG_TDH, 0);
            Self::mmio_write(mmio_base, REG_TDT, 0);
            Self::mmio_write(mmio_base, REG_TIPG, 10 | (8 << 10) | (6 << 20));
        }

        let tctl = TCTL_EN | TCTL_PSP | (0x0F << TCTL_CT_SHIFT) | (0x40 << TCTL_COLD_SHIFT);
        unsafe {
            Self::mmio_write(mmio_base, REG_TCTL, tctl);
        }

        Ok(Self {
            pci,
            mmio_base,
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
        let status = unsafe { Self::mmio_read(self.mmio_base, REG_STATUS) };
        (status & (1 << 1)) != 0
    }

    pub fn transmit(&mut self, packet: &[u8]) {
        assert!(packet.len() <= BUFFER_SIZE);
        let desc = &mut self.tx_descs[self.tx_cur];

        let mut iters = 0;
        while (unsafe { core::ptr::read_volatile(&desc.status) } & 1) == 0 {
            core::hint::spin_loop();
            iters += 1;
            if iters > 10_000 {
                return;
            }
        }

        let buf_ptr = self.tx_bufs[self.tx_cur] as *mut u8;
        unsafe {
            core::ptr::copy_nonoverlapping(packet.as_ptr(), buf_ptr, packet.len());
            core::ptr::write_volatile(&mut desc.length, packet.len() as u16);
            core::ptr::write_volatile(&mut desc.cso, 0);
            core::ptr::write_volatile(&mut desc.cmd, CMD_EOP | CMD_IFCS | CMD_RS);
            core::ptr::write_volatile(&mut desc.status, 0);
            core::ptr::write_volatile(&mut desc.css, 0);
            core::ptr::write_volatile(&mut desc.special, 0);
        }
        compiler_fence(Ordering::SeqCst);

        self.tx_cur = (self.tx_cur + 1) % NUM_TX_DESC;
        unsafe {
            Self::mmio_write(self.mmio_base, REG_TDT, self.tx_cur as u32);
        }
    }

    pub fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        let desc = &mut self.rx_descs[self.rx_cur];
        let status = unsafe { core::ptr::read_volatile(&desc.status) };
        if (status & 1) == 0 {
            return None;
        }

        let errors = unsafe { core::ptr::read_volatile(&desc.errors) };
        if errors != 0 || (status & 2) == 0 {
            unsafe {
                core::ptr::write_volatile(&mut desc.status, 0);
            }
            compiler_fence(Ordering::SeqCst);
            let old_cur = self.rx_cur;
            self.rx_cur = (self.rx_cur + 1) % NUM_RX_DESC;
            unsafe {
                Self::mmio_write(self.mmio_base, REG_RDT, old_cur as u32);
            }
            return None;
        }

        compiler_fence(Ordering::SeqCst);
        let length = unsafe { core::ptr::read_volatile(&desc.length) } as usize;
        let copy_len = length.min(buf.len());
        let buf_ptr = self.rx_bufs[self.rx_cur] as *const u8;
        unsafe {
            core::ptr::copy_nonoverlapping(buf_ptr, buf.as_mut_ptr(), copy_len);
            core::ptr::write_volatile(&mut desc.status, 0);
        }
        compiler_fence(Ordering::SeqCst);

        let old_cur = self.rx_cur;
        self.rx_cur = (self.rx_cur + 1) % NUM_RX_DESC;
        unsafe {
            Self::mmio_write(self.mmio_base, REG_RDT, old_cur as u32);
        }

        Some(copy_len)
    }

    pub fn has_packets(&self) -> bool {
        let desc = &self.rx_descs[self.rx_cur];
        let status = unsafe { core::ptr::read_volatile(&desc.status) };
        (status & 1) != 0
    }

    unsafe fn mmio_read(base: u64, offset: usize) -> u32 {
        unsafe { core::ptr::read_volatile((base + offset as u64) as *const u32) }
    }

    unsafe fn mmio_write(base: u64, offset: usize, val: u32) {
        unsafe { core::ptr::write_volatile((base + offset as u64) as *mut u32, val) }
    }

    fn read_eeprom(base: u64, addr: u8) -> u16 {
        // Try e1000 format: start=bit 0, addr=bits 8..15, done=bit 4, data=bits 16..31
        unsafe {
            Self::mmio_write(base, REG_EERD, 1 | ((addr as u32) << 8));
        }
        for _ in 0..10_000 {
            let val = unsafe { Self::mmio_read(base, REG_EERD) };
            if (val & (1 << 4)) != 0 {
                return (val >> 16) as u16;
            }
            core::hint::spin_loop();
        }

        // Try e1000e format: start=bit 0, addr=bits 2..15, done=bit 1, data=bits 16..31
        unsafe {
            Self::mmio_write(base, REG_EERD, 1 | ((addr as u32) << 2));
        }
        for _ in 0..10_000 {
            let val = unsafe { Self::mmio_read(base, REG_EERD) };
            if (val & (1 << 1)) != 0 {
                return (val >> 16) as u16;
            }
            core::hint::spin_loop();
        }
        0
    }
}

pub fn probe(pci: &PciDevice) -> bool {
    pci.vendor_id == 0x8086 && pci.class_code == 0x02 && pci.subclass == 0x00
}
