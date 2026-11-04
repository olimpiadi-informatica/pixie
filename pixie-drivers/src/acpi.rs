use alloc::vec;

use crate::hal::Hal;

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
    let (chunks, tail) = dst.as_chunks_mut::<4>();
    for (i, chunk) in chunks.iter_mut().enumerate() {
        *chunk = hal.mmio_read32(phys + (i * 4) as u64).to_le_bytes();
    }
    let tail_base = phys + (chunks.len() * 4) as u64;
    for (j, b) in tail.iter_mut().enumerate() {
        *b = hal.mmio_read8(tail_base + j as u64);
    }
}

fn read_u32_le<H: Hal>(hal: &H, phys: u64) -> u32 {
    u32::from_le(hal.mmio_read32(phys))
}

fn read_u64_le<H: Hal>(hal: &H, phys: u64) -> u64 {
    (read_u32_le(hal, phys) as u64) | ((read_u32_le(hal, phys + 4) as u64) << 32)
}

fn find_facp_in_sdt<H: Hal>(
    hal: &H,
    sdt_phys: u64,
    expected_sig: &[u8; 4],
    entry_size: usize,
) -> Option<u64> {
    if sdt_phys == 0 {
        return None;
    }
    hal.map_mmio(sdt_phys, 4096);
    if &read_u32_le(hal, sdt_phys).to_le_bytes() != expected_sig {
        return None;
    }
    let len = read_u32_le(hal, sdt_phys + 4) as usize;
    if !(36..=65536).contains(&len) {
        return None;
    }
    hal.map_mmio(sdt_phys, len);
    let count = (len - 36) / entry_size;
    for i in 0..count {
        let entry_addr = sdt_phys + 36 + (i * entry_size) as u64;
        let entry = if entry_size == 8 {
            read_u64_le(hal, entry_addr)
        } else {
            read_u32_le(hal, entry_addr) as u64
        };
        if entry == 0 {
            continue;
        }
        hal.map_mmio(entry, 4096);
        if &read_u32_le(hal, entry).to_le_bytes() == b"FACP" {
            return Some(entry);
        }
    }
    None
}

fn read_fadt_gas_io_port<H: Hal>(
    hal: &H,
    fadt_phys: u64,
    fadt_rev: u8,
    fadt_len: usize,
    legacy_off: u64,
    x_gas_off: u64,
) -> u16 {
    let legacy = read_u32_le(hal, fadt_phys + legacy_off) as u16;
    if fadt_rev >= 2 && fadt_len >= (x_gas_off as usize + 12) {
        let space_id = hal.mmio_read8(fadt_phys + x_gas_off);
        let addr = read_u64_le(hal, fadt_phys + x_gas_off + 4);
        if space_id == 1 && addr != 0 {
            return addr as u16;
        }
    }
    legacy
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
    if aml.len() < 7 {
        return None;
    }
    for i in 1..aml.len().saturating_sub(5) {
        if &aml[i..i + 4] != b"_S5_" {
            continue;
        }
        // Require NameOp (0x08) directly before `_S5_` or before root prefix `\_S5_`.
        let has_name_op =
            aml[i - 1] == 0x08 || (i >= 2 && aml[i - 1] == b'\\' && aml[i - 2] == 0x08);
        if !has_name_op {
            continue;
        }
        let mut p = i + 4;
        if aml.get(p) != Some(&0x12) {
            if aml.get(p + 1) == Some(&0x12) {
                p += 1;
            } else {
                continue;
            }
        }
        // p points to PackageOp (0x12)
        let Some(&lead) = aml.get(p + 1) else {
            continue;
        };
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
        if rsdp[..20].iter().fold(0u8, |acc, &b| acc.wrapping_add(b)) != 0 {
            return None;
        }

        let revision = rsdp[15];
        let rsdt_phys = u32::from_le_bytes(rsdp[16..20].try_into().ok()?) as u64;
        let xsdt_phys = if revision >= 2 {
            read_bytes(hal, rsdp_phys + 20, &mut rsdp[20..36]);
            if rsdp.iter().fold(0u8, |acc, &b| acc.wrapping_add(b)) == 0 {
                u64::from_le_bytes(rsdp[24..32].try_into().ok()?)
            } else {
                0
            }
        } else {
            0
        };

        let fadt_phys = if revision >= 2 && xsdt_phys != 0 {
            find_facp_in_sdt(hal, xsdt_phys, b"XSDT", 8)
        } else {
            None
        }
        .or_else(|| find_facp_in_sdt(hal, rsdt_phys, b"RSDT", 4))?;

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

        let pm1a_cnt_blk = read_fadt_gas_io_port(hal, fadt_phys, fadt_rev, fadt_len, 64, 172);
        if pm1a_cnt_blk == 0 {
            return None;
        }

        let pm1b_raw = read_fadt_gas_io_port(hal, fadt_phys, fadt_rev, fadt_len, 68, 184);
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
        if &read_u32_le(hal, dsdt_phys).to_le_bytes() != b"DSDT" {
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
