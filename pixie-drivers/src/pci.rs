use alloc::vec::Vec;

use crate::hal::Hal;

pub const PCI_CONFIG_ADDRESS: u16 = 0xCF8;
pub const PCI_CONFIG_DATA: u16 = 0xCFC;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[inline]
fn config_addr(bus: u8, dev: u8, func: u8, offset: u8) -> u32 {
    0x8000_0000u32
        | ((bus as u32) << 16)
        | (((dev as u32) & 0x1F) << 11)
        | (((func as u32) & 0x07) << 8)
        | ((offset as u32) & 0xFC)
}

pub fn pci_read_u32<H: Hal>(hal: &H, bus: u8, dev: u8, func: u8, offset: u8) -> u32 {
    hal.io_write32(PCI_CONFIG_ADDRESS, config_addr(bus, dev, func, offset));
    hal.io_read32(PCI_CONFIG_DATA)
}

pub fn pci_write_u32<H: Hal>(hal: &H, bus: u8, dev: u8, func: u8, offset: u8, val: u32) {
    hal.io_write32(PCI_CONFIG_ADDRESS, config_addr(bus, dev, func, offset));
    hal.io_write32(PCI_CONFIG_DATA, val);
}

pub fn pci_read_u16<H: Hal>(hal: &H, bus: u8, dev: u8, func: u8, offset: u8) -> u16 {
    let dword = pci_read_u32(hal, bus, dev, func, offset);
    ((dword >> ((offset & 2) * 8)) & 0xFFFF) as u16
}

pub fn pci_write_u16<H: Hal>(hal: &H, bus: u8, dev: u8, func: u8, offset: u8, val: u16) {
    // Use native 16-bit config data port write so adjacent RW1C bits (e.g. PCI Status at 0x06
    // when writing Command at 0x04) are never clobbered by a 32-bit RMW.
    hal.io_write32(PCI_CONFIG_ADDRESS, config_addr(bus, dev, func, offset));
    hal.io_write16(PCI_CONFIG_DATA + ((offset & 2) as u16), val);
}

pub fn pci_read_u8<H: Hal>(hal: &H, bus: u8, dev: u8, func: u8, offset: u8) -> u8 {
    let dword = pci_read_u32(hal, bus, dev, func, offset);
    ((dword >> ((offset & 3) * 8)) & 0xFF) as u8
}

pub fn pci_write_u8<H: Hal>(hal: &H, bus: u8, dev: u8, func: u8, offset: u8, val: u8) {
    hal.io_write32(PCI_CONFIG_ADDRESS, config_addr(bus, dev, func, offset));
    hal.io_write8(PCI_CONFIG_DATA + ((offset & 3) as u16), val);
}

fn delay_us<H: Hal>(hal: &H, us: i64) {
    let start = hal.micros();
    let mut last = start;
    let mut stalled = 0usize;
    loop {
        let now = hal.micros();
        if now.saturating_sub(start) >= us {
            break;
        }
        if now == last {
            stalled += 1;
            if stalled >= 50_000 {
                break;
            }
        } else {
            last = now;
            stalled = 0;
        }
        core::hint::spin_loop();
    }
}

impl PciDevice {
    pub fn read_u32<H: Hal>(&self, hal: &H, offset: u8) -> u32 {
        pci_read_u32(hal, self.bus, self.dev, self.func, offset)
    }

    pub fn write_u32<H: Hal>(&self, hal: &H, offset: u8, val: u32) {
        pci_write_u32(hal, self.bus, self.dev, self.func, offset, val);
    }

    pub fn read_u16<H: Hal>(&self, hal: &H, offset: u8) -> u16 {
        pci_read_u16(hal, self.bus, self.dev, self.func, offset)
    }

    pub fn write_u16<H: Hal>(&self, hal: &H, offset: u8, val: u16) {
        pci_write_u16(hal, self.bus, self.dev, self.func, offset, val);
    }

    pub fn read_u8<H: Hal>(&self, hal: &H, offset: u8) -> u8 {
        pci_read_u8(hal, self.bus, self.dev, self.func, offset)
    }

    pub fn write_u8<H: Hal>(&self, hal: &H, offset: u8, val: u8) {
        pci_write_u8(hal, self.bus, self.dev, self.func, offset, val);
    }

    pub fn enable_bus_mastering<H: Hal>(&self, hal: &H) {
        // Wake device from D3hot to D0 if PCI Power Management capability (ID 0x01) is present.
        let status = self.read_u16(hal, 0x06);
        if status != 0xFFFF && (status & 0x0010) != 0 {
            let mut cap_ptr = self.read_u8(hal, 0x34) & !0x03;
            for _ in 0..48 {
                if cap_ptr < 0x40 || cap_ptr == 0xFC {
                    break;
                }
                let cap_id = self.read_u8(hal, cap_ptr);
                let next_ptr = self.read_u8(hal, cap_ptr.wrapping_add(1)) & !0x03;
                if cap_id == 0x01 {
                    let pmcsr_offset = cap_ptr.wrapping_add(4);
                    let pmcsr = self.read_u16(hal, pmcsr_offset);
                    if (pmcsr & 0x0003) != 0 {
                        self.write_u16(hal, pmcsr_offset, pmcsr & !0x0003);
                        let _ = self.read_u16(hal, pmcsr_offset);
                        // PCI PM Spec §5.6.1 requires >= 10 ms recovery time after D3hot -> D0.
                        delay_us(hal, 10_000);
                    }
                    break;
                }
                cap_ptr = next_ptr;
            }
        }

        let cmd = self.read_u16(hal, 0x04);
        self.write_u16(hal, 0x04, cmd | 0x0007);
    }

    pub fn read_bar<H: Hal>(&self, hal: &H, bar_idx: u8) -> Option<Bar> {
        let max_bars: u8 = match self.read_u8(hal, 0x0E) & 0x7F {
            0x00 => 6,
            0x01 => 2,
            _ => return None,
        };
        if bar_idx >= max_bars {
            return None;
        }

        // Walk preceding BAR slots so the upper 32-bit half of a 64-bit Memory BAR is skipped.
        let mut idx = 0u8;
        while idx < bar_idx {
            let bar_val = self.read_u32(hal, 0x10 + idx * 4);
            let is_64bit_mem =
                bar_val != 0xFFFF_FFFF && (bar_val & 1) == 0 && ((bar_val >> 1) & 0x03) == 0x02;
            if is_64bit_mem {
                if idx + 1 == bar_idx {
                    return None;
                }
                idx += 2;
            } else {
                idx += 1;
            }
        }

        let offset = 0x10 + bar_idx * 4;
        let orig = self.read_u32(hal, offset);
        if orig == 0 || orig == 0xFFFF_FFFF {
            return None;
        }
        if (orig & 1) != 0 {
            let port = (orig & 0xFFFC) as u16;
            return (port != 0).then_some(Bar::Io { port });
        }
        let is_64bit = ((orig >> 1) & 0x03) == 0x02;
        let (orig_high, base) = if is_64bit {
            if bar_idx + 1 >= max_bars {
                return None;
            }
            let orig_high = self.read_u32(hal, offset + 4);
            if orig_high == 0xFFFF_FFFF {
                return None;
            }
            (
                orig_high,
                ((orig_high as u64) << 32) | ((orig as u64) & 0xFFFF_FFF0),
            )
        } else {
            (0, (orig as u64) & 0xFFFF_FFF0)
        };
        if base == 0 {
            return None;
        }

        // Probe BAR size with MEM/IO decode temporarily disabled.
        let orig_cmd = self.read_u16(hal, 0x04);
        if (orig_cmd & 0x0003) != 0 {
            self.write_u16(hal, 0x04, orig_cmd & !0x0003);
        }
        self.write_u32(hal, offset, 0xFFFF_FFFF);
        let mask_lo = self.read_u32(hal, offset);
        self.write_u32(hal, offset, orig);

        let size = if is_64bit {
            self.write_u32(hal, offset + 4, 0xFFFF_FFFF);
            let mask_hi = self.read_u32(hal, offset + 4);
            self.write_u32(hal, offset + 4, orig_high);
            if (mask_lo == 0xFFFF_FFFF && mask_hi == 0xFFFF_FFFF)
                || (mask_lo == orig && mask_hi == orig_high)
            {
                0
            } else {
                let mask = ((mask_hi as u64) << 32) | ((mask_lo as u64) & !0xFu64);
                let sz = (!mask).wrapping_add(1);
                usize::try_from(sz).unwrap_or(0)
            }
        } else if mask_lo == 0 || mask_lo == 0xFFFF_FFFF || mask_lo == orig {
            0
        } else {
            let mask = mask_lo & !0xFu32;
            if mask == 0 {
                0
            } else {
                (!mask).wrapping_add(1) as usize
            }
        };

        if (orig_cmd & 0x0003) != 0 {
            self.write_u16(hal, 0x04, orig_cmd);
        }

        Some(Bar::Memory {
            base,
            size,
            is_64bit,
        })
    }
}

pub fn scan_pci<H: Hal>(hal: &H) -> Vec<PciDevice> {
    let mut devices = Vec::new();
    for bus in 0..=255 {
        for dev in 0..32 {
            let vendor_id = pci_read_u16(hal, bus, dev, 0, 0x00);
            if vendor_id == 0xFFFF || vendor_id == 0x0000 {
                continue;
            }
            let header_type = pci_read_u8(hal, bus, dev, 0, 0x0E);
            let num_functions = if (header_type & 0x80) != 0 { 8 } else { 1 };
            for func in 0..num_functions {
                let vendor_id = pci_read_u16(hal, bus, dev, func, 0x00);
                if vendor_id == 0xFFFF || vendor_id == 0x0000 {
                    continue;
                }
                let device_id = pci_read_u16(hal, bus, dev, func, 0x02);
                let class_code = pci_read_u8(hal, bus, dev, func, 0x0B);
                let subclass = pci_read_u8(hal, bus, dev, func, 0x0A);
                let prog_if = pci_read_u8(hal, bus, dev, func, 0x09);
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
