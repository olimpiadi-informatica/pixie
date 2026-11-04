use crate::hal::Hal;
use alloc::vec;

/// Parsed ACPI power management and reset register information extracted from RSDP -> RSDT/XSDT -> FADT + DSDT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcpiInfo {
    pub pm1a_cnt_blk: u16,
    pub pm1b_cnt_blk: Option<u16>,
    pub slp_typa: u16,
    pub slp_typb: u16,
    pub reset_io_port: Option<u16>,
    pub reset_mmio_addr: Option<u64>,
    pub reset_value: u8,
}

fn read_bytes<H: Hal>(hal: &H, phys: u64, dst: &mut [u8]) {
    for (i, b) in dst.iter_mut().enumerate() {
        *b = hal.mmio_read8(phys + i as u64);
    }
}

fn read_u16_le<H: Hal>(hal: &H, phys: u64) -> u16 {
    let mut b = [0u8; 2];
    read_bytes(hal, phys, &mut b);
    u16::from_le_bytes(b)
}

fn read_u32_le<H: Hal>(hal: &H, phys: u64) -> u32 {
    let mut b = [0u8; 4];
    read_bytes(hal, phys, &mut b);
    u32::from_le_bytes(b)
}

fn read_u64_le<H: Hal>(hal: &H, phys: u64) -> u64 {
    let mut b = [0u8; 8];
    read_bytes(hal, phys, &mut b);
    u64::from_le_bytes(b)
}

fn decode_aml_int(aml: &[u8], cur: &mut usize) -> Option<u16> {
    let op = *aml.get(*cur)?;
    match op {
        0x00 => {
            *cur += 1;
            Some(0)
        }
        0x01 => {
            *cur += 1;
            Some(1)
        }
        0xFF => {
            *cur += 1;
            Some(0xFFFF)
        }
        0x0A => {
            let val = *aml.get(*cur + 1)? as u16;
            *cur += 2;
            Some(val)
        }
        0x0B => {
            let lo = *aml.get(*cur + 1)? as u16;
            let hi = *aml.get(*cur + 2)? as u16;
            *cur += 3;
            Some(lo | (hi << 8))
        }
        0x0C => {
            let bytes = aml.get(*cur + 1..*cur + 5)?;
            let val = u32::from_le_bytes(bytes.try_into().ok()?) as u16;
            *cur += 5;
            Some(val)
        }
        0x0E => {
            let bytes = aml.get(*cur + 1..*cur + 9)?;
            let val = u64::from_le_bytes(bytes.try_into().ok()?) as u16;
            *cur += 9;
            Some(val)
        }
        0x02..=0x07 => {
            *cur += 1;
            Some(op as u16)
        }
        _ => None,
    }
}

fn parse_s5_from_aml(aml: &[u8]) -> Option<(u16, u16)> {
    if aml.len() < 6 {
        return None;
    }
    for i in 0..aml.len().saturating_sub(5) {
        if &aml[i..i + 4] != b"_S5_" {
            continue;
        }
        // Check that this is either preceded by NameOp (0x08) or at start / after root prefix '\'
        let mut p = i + 4;
        if aml.get(p) != Some(&0x12) {
            if aml.get(p + 1) == Some(&0x12) {
                p += 1;
            } else {
                continue;
            }
        }
        // p points to PackageOp (0x12)
        let lead = *aml.get(p + 1)?;
        let pkg_len_bytes = match lead >> 6 {
            0 => 1,
            1 => 2,
            2 => 3,
            _ => 4,
        };
        // After PkgLength is NumElements (1 byte)
        let mut cur = p + 1 + pkg_len_bytes + 1;
        if cur >= aml.len() {
            continue;
        }
        if let Some(slp_typa) = decode_aml_int(aml, &mut cur) {
            let slp_typb = decode_aml_int(aml, &mut cur).unwrap_or(slp_typa);
            return Some((slp_typa & 0x07, slp_typb & 0x07));
        }
    }
    None
}

impl AcpiInfo {
    /// Parses the ACPI RSDP at `rsdp_phys`, walks XSDT (for revision >= 2 when `xsdt_address != 0`)
    /// or RSDT, finds the `FACP` (FADT) table, extracts `PM1a_CNT_BLK`, `PM1b_CNT_BLK`, `RESET_REG` /
    /// `RESET_VALUE` (if `FLAGS` bit 10 is set or `reset_reg` is populated in v2+), and scans the DSDT
    /// AML bytecode for the `_S5_` sleep package to determine `SLP_TYPa` and `SLP_TYPb`.
    pub fn parse_from_rsdp<H: Hal>(rsdp_phys: u64, hal: &H) -> Option<Self> {
        if rsdp_phys == 0 {
            return None;
        }
        hal.map_mmio(rsdp_phys, 4096);

        let mut rsdp = [0u8; 36];
        read_bytes(hal, rsdp_phys, &mut rsdp[..20]);
        if &rsdp[0..8] != b"RSD PTR " {
            return None;
        }

        let revision = rsdp[15];
        let rsdt_phys = u32::from_le_bytes(rsdp[16..20].try_into().ok()?) as u64;
        let xsdt_phys = if revision >= 2 {
            read_bytes(hal, rsdp_phys + 20, &mut rsdp[20..36]);
            u64::from_le_bytes(rsdp[24..32].try_into().ok()?)
        } else {
            0
        };

        let mut fadt_phys = None;

        if revision >= 2 && xsdt_phys != 0 {
            hal.map_mmio(xsdt_phys, 4096);
            let mut sig = [0u8; 4];
            read_bytes(hal, xsdt_phys, &mut sig);
            if &sig == b"XSDT" {
                let len = read_u32_le(hal, xsdt_phys + 4) as usize;
                if (36..=65536).contains(&len) {
                    hal.map_mmio(xsdt_phys, len);
                    let count = (len - 36) / 8;
                    for i in 0..count {
                        let entry = read_u64_le(hal, xsdt_phys + 36 + (i * 8) as u64);
                        if entry == 0 {
                            continue;
                        }
                        hal.map_mmio(entry, 4096);
                        let mut esig = [0u8; 4];
                        read_bytes(hal, entry, &mut esig);
                        if &esig == b"FACP" {
                            fadt_phys = Some(entry);
                            break;
                        }
                    }
                }
            }
        }

        if fadt_phys.is_none() && rsdt_phys != 0 {
            hal.map_mmio(rsdt_phys, 4096);
            let mut sig = [0u8; 4];
            read_bytes(hal, rsdt_phys, &mut sig);
            if &sig == b"RSDT" {
                let len = read_u32_le(hal, rsdt_phys + 4) as usize;
                if (36..=65536).contains(&len) {
                    hal.map_mmio(rsdt_phys, len);
                    let count = (len - 36) / 4;
                    for i in 0..count {
                        let entry = read_u32_le(hal, rsdt_phys + 36 + (i * 4) as u64) as u64;
                        if entry == 0 {
                            continue;
                        }
                        hal.map_mmio(entry, 4096);
                        let mut esig = [0u8; 4];
                        read_bytes(hal, entry, &mut esig);
                        if &esig == b"FACP" {
                            fadt_phys = Some(entry);
                            break;
                        }
                    }
                }
            }
        }

        let fadt_phys = fadt_phys?;
        let fadt_len = read_u32_le(hal, fadt_phys + 4) as usize;
        if fadt_len < 72 {
            return None;
        }
        hal.map_mmio(fadt_phys, fadt_len);
        let fadt_rev = hal.mmio_read8(fadt_phys + 8);

        let dsdt_32 = read_u32_le(hal, fadt_phys + 40) as u64;
        let dsdt_phys = if fadt_rev >= 2 && fadt_len >= 148 {
            let x_dsdt = read_u64_le(hal, fadt_phys + 140);
            if x_dsdt != 0 { x_dsdt } else { dsdt_32 }
        } else {
            dsdt_32
        };

        let pm1a_32 = read_u32_le(hal, fadt_phys + 64) as u16;
        let pm1a_cnt_blk = if fadt_rev >= 2 && fadt_len >= 184 {
            let space_id = hal.mmio_read8(fadt_phys + 172);
            let addr = read_u64_le(hal, fadt_phys + 172 + 4);
            if space_id == 1 && addr != 0 {
                addr as u16
            } else {
                pm1a_32
            }
        } else {
            pm1a_32
        };
        if pm1a_cnt_blk == 0 {
            return None;
        }

        let pm1b_32 = read_u32_le(hal, fadt_phys + 68) as u16;
        let pm1b_raw = if fadt_rev >= 2 && fadt_len >= 196 {
            let space_id = hal.mmio_read8(fadt_phys + 184);
            let addr = read_u64_le(hal, fadt_phys + 184 + 4);
            if space_id == 1 && addr != 0 {
                addr as u16
            } else {
                pm1b_32
            }
        } else {
            pm1b_32
        };
        let pm1b_cnt_blk = (pm1b_raw != 0).then_some(pm1b_raw);

        let mut reset_io_port = None;
        let mut reset_mmio_addr = None;
        let mut reset_value = 0u8;

        if fadt_len >= 129 {
            let flags = read_u32_le(hal, fadt_phys + 112);
            let space_id = hal.mmio_read8(fadt_phys + 116);
            let reset_addr = read_u64_le(hal, fadt_phys + 116 + 4);
            let val = hal.mmio_read8(fadt_phys + 128);
            if reset_addr != 0 && ((flags & (1 << 10)) != 0 || fadt_rev >= 2) {
                reset_value = val;
                if space_id == 1 {
                    reset_io_port = Some(reset_addr as u16);
                } else if space_id == 0 {
                    reset_mmio_addr = Some(reset_addr);
                }
            }
        }

        if dsdt_phys == 0 {
            return None;
        }
        hal.map_mmio(dsdt_phys, 4096);
        let mut dsig = [0u8; 4];
        read_bytes(hal, dsdt_phys, &mut dsig);
        if &dsig != b"DSDT" {
            return None;
        }
        let dsdt_len = read_u32_le(hal, dsdt_phys + 4) as usize;
        if !(36..=1024 * 1024).contains(&dsdt_len) {
            return None;
        }
        hal.map_mmio(dsdt_phys, dsdt_len);
        let aml_len = dsdt_len - 36;
        let mut aml = vec![0u8; aml_len];
        read_bytes(hal, dsdt_phys + 36, &mut aml);

        let (slp_typa, slp_typb) = parse_s5_from_aml(&aml)?;

        let _ = read_u16_le(hal, fadt_phys);

        Some(Self {
            pm1a_cnt_blk,
            pm1b_cnt_blk,
            slp_typa,
            slp_typb,
            reset_io_port,
            reset_mmio_addr,
            reset_value,
        })
    }

    /// Commands an ACPI S5 soft-off by writing `(slp_typa << 10) | (1 << 13)` to `pm1a_cnt_blk`
    /// (and `slp_typb` to `pm1b_cnt_blk` if present).
    pub fn shutdown<H: Hal>(&self, hal: &H) {
        let val_a = ((self.slp_typa & 0x07) << 10) | (1 << 13);
        hal.io_write16(self.pm1a_cnt_blk, val_a);
        if let Some(pm1b) = self.pm1b_cnt_blk {
            let val_b = ((self.slp_typb & 0x07) << 10) | (1 << 13);
            hal.io_write16(pm1b, val_b);
        }
    }

    /// Commands an ACPI system reset via `reset_io_port` or `reset_mmio_addr` if present.
    pub fn reset<H: Hal>(&self, hal: &H) -> bool {
        if let Some(port) = self.reset_io_port {
            hal.io_write8(port, self.reset_value);
            return true;
        }
        if let Some(addr) = self.reset_mmio_addr {
            hal.map_mmio(addr, 4);
            hal.mmio_write8(addr, self.reset_value);
            return true;
        }
        false
    }
}
