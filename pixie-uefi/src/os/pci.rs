use alloc::vec::Vec;

use crate::os::arch::io;

const PCI_CONFIG_ADDRESS: u16 = 0xCF8;
const PCI_CONFIG_DATA: u16 = 0xCFC;

#[derive(Debug, Clone, Copy)]
pub struct PciDevice {
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub class_code: u8,
    pub subclass: u8,
    pub prog_if: u8,
}

#[derive(Debug, Clone, Copy)]
pub enum Bar {
    Memory {
        base: u64,
        size: usize,
        is_64bit: bool,
    },
    Io {
        port: u16,
    },
}

pub fn pci_read_u32(bus: u8, dev: u8, func: u8, offset: u8) -> u32 {
    let address = 0x8000_0000u32
        | ((bus as u32) << 16)
        | ((dev as u32) << 11)
        | ((func as u32) << 8)
        | ((offset as u32) & 0xFC);
    unsafe {
        io::outl(PCI_CONFIG_ADDRESS, address);
        io::inl(PCI_CONFIG_DATA)
    }
}

pub fn pci_write_u32(bus: u8, dev: u8, func: u8, offset: u8, val: u32) {
    let address = 0x8000_0000u32
        | ((bus as u32) << 16)
        | ((dev as u32) << 11)
        | ((func as u32) << 8)
        | ((offset as u32) & 0xFC);
    unsafe {
        io::outl(PCI_CONFIG_ADDRESS, address);
        io::outl(PCI_CONFIG_DATA, val);
    }
}

pub fn pci_read_u16(bus: u8, dev: u8, func: u8, offset: u8) -> u16 {
    let dword = pci_read_u32(bus, dev, func, offset);
    ((dword >> ((offset & 2) * 8)) & 0xFFFF) as u16
}

pub fn pci_write_u16(bus: u8, dev: u8, func: u8, offset: u8, val: u16) {
    let shift = (offset & 2) * 8;
    let dword = pci_read_u32(bus, dev, func, offset);
    let new_dword = (dword & !(0xFFFF << shift)) | ((val as u32) << shift);
    pci_write_u32(bus, dev, func, offset, new_dword);
}

pub fn pci_read_u8(bus: u8, dev: u8, func: u8, offset: u8) -> u8 {
    let dword = pci_read_u32(bus, dev, func, offset);
    ((dword >> ((offset & 3) * 8)) & 0xFF) as u8
}

impl PciDevice {
    pub fn read_u32(&self, offset: u8) -> u32 {
        pci_read_u32(self.bus, self.dev, self.func, offset)
    }

    pub fn write_u32(&self, offset: u8, val: u32) {
        pci_write_u32(self.bus, self.dev, self.func, offset, val)
    }

    pub fn read_u16(&self, offset: u8) -> u16 {
        pci_read_u16(self.bus, self.dev, self.func, offset)
    }

    pub fn write_u16(&self, offset: u8, val: u16) {
        pci_write_u16(self.bus, self.dev, self.func, offset, val)
    }

    pub fn enable_bus_mastering(&self) {
        let cmd = self.read_u16(0x04);
        // Bit 0: I/O Space, Bit 1: Memory Space, Bit 2: Bus Master
        self.write_u16(0x04, cmd | 0x0007);
    }

    pub fn read_bar(&self, bar_idx: u8) -> Option<Bar> {
        if bar_idx > 5 {
            return None;
        }
        let offset = 0x10 + bar_idx * 4;
        let orig = self.read_u32(offset);

        if (orig & 1) != 0 {
            // I/O Space
            let port = (orig & 0xFFFC) as u16;
            return Some(Bar::Io { port });
        }

        // Memory space
        let is_64bit = ((orig >> 1) & 0x03) == 0x02;

        let base = if is_64bit {
            if bar_idx >= 5 {
                return None;
            }
            let orig_high = self.read_u32(offset + 4);
            ((orig_high as u64) << 32) | ((orig as u64) & 0xFFFF_FFF0)
        } else {
            (orig as u64) & 0xFFFF_FFF0
        };

        Some(Bar::Memory {
            base,
            size: 0,
            is_64bit,
        })
    }
}

pub fn scan_pci() -> Vec<PciDevice> {
    let mut devices = Vec::new();

    for bus in 0..=255 {
        for dev in 0..32 {
            let vendor_id = pci_read_u16(bus, dev, 0, 0x00);
            if vendor_id == 0xFFFF {
                continue;
            }

            let header_type = pci_read_u8(bus, dev, 0, 0x0E);
            let num_functions = if (header_type & 0x80) != 0 { 8 } else { 1 };

            for func in 0..num_functions {
                let vendor_id = pci_read_u16(bus, dev, func, 0x00);
                if vendor_id == 0xFFFF {
                    continue;
                }

                let device_id = pci_read_u16(bus, dev, func, 0x02);
                let class_code = pci_read_u8(bus, dev, func, 0x0B);
                let subclass = pci_read_u8(bus, dev, func, 0x0A);
                let prog_if = pci_read_u8(bus, dev, func, 0x09);

                devices.push(PciDevice {
                    bus,
                    dev,
                    func,
                    vendor_id,
                    device_id,
                    class_code,
                    subclass,
                    prog_if,
                });
            }
        }
    }

    devices
}
