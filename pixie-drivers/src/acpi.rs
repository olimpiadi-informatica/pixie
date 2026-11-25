use alloc::vec;
use alloc::vec::Vec;

use crate::hal::Hal;

/// Parsed ACPI power management and reset register information extracted from RSDP -> RSDT/XSDT -> FADT + DSDT/SSDT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcpiInfo {
    pub pm1a_cnt_blk: u16,
    pub pm1b_cnt_blk: Option<u16>,
    pub slp_typa: u16,
    pub slp_typb: u16,
    pub reset_io_port: Option<u16>,
    pub reset_mmio_addr: Option<u64>,
    pub reset_value: u8,
    pub smi_cmd: u16,
    pub acpi_enable: u8,
}

fn read_bytes<H: Hal>(hal: &H, phys: u64, dst: &mut [u8]) {
    for (i, b) in dst.iter_mut().enumerate() {
        *b = hal.mmio_read8(phys + i as u64);
    }
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

fn verify_table_checksum<H: Hal>(hal: &H, phys: u64, len: usize) -> bool {
    let mut sum = 0u8;
    for i in 0..len {
        sum = sum.wrapping_add(hal.mmio_read8(phys + i as u64));
    }
    sum == 0
}

fn collect_sdt_entries<H: Hal>(
    hal: &H,
    sdt_phys: u64,
    expected_sig: &[u8; 4],
    entry_size: usize,
) -> Option<Vec<u64>> {
    if sdt_phys == 0 {
        return None;
    }
    hal.map_mmio(sdt_phys, 4096);
    let mut sig = [0u8; 4];
    read_bytes(hal, sdt_phys, &mut sig);
    if &sig != expected_sig {
        return None;
    }
    let len = read_u32_le(hal, sdt_phys + 4) as usize;
    if !(36..=65536).contains(&len) {
        return None;
    }
    hal.map_mmio(sdt_phys, len);
    if !verify_table_checksum(hal, sdt_phys, len) {
        return None;
    }
    let count = (len - 36) / entry_size;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let entry_addr = sdt_phys + 36 + (i * entry_size) as u64;
        let entry = if entry_size == 8 {
            read_u64_le(hal, entry_addr)
        } else {
            read_u32_le(hal, entry_addr) as u64
        };
        if entry != 0 {
            out.push(entry);
        }
    }
    Some(out)
}

fn find_table_in_entries<H: Hal>(hal: &H, entries: &[u64], sig_target: &[u8; 4]) -> Option<u64> {
    for &entry in entries {
        hal.map_mmio(entry, 4096);
        let mut sig = [0u8; 4];
        read_bytes(hal, entry, &mut sig);
        if &sig == sig_target {
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
    let legacy = u16::try_from(read_u32_le(hal, fadt_phys + legacy_off)).unwrap_or(0);
    if fadt_rev >= 2 && fadt_len >= (x_gas_off as usize + 12) {
        let space_id = hal.mmio_read8(fadt_phys + x_gas_off);
        let addr = read_u64_le(hal, fadt_phys + x_gas_off + 4);
        if space_id == 1
            && addr != 0
            && let Ok(port) = u16::try_from(addr)
        {
            return port;
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
        _ => None,
    }
}

fn decode_pkg_length(aml: &[u8], cur: &mut usize) -> Option<usize> {
    let lead = *aml.get(*cur)?;
    let byte_count = (lead >> 6) as usize;
    if byte_count == 0 {
        *cur += 1;
        Some((lead & 0x3F) as usize)
    } else {
        if *cur + 1 + byte_count > aml.len() {
            return None;
        }
        let mut len = (lead & 0x0F) as usize;
        for i in 0..byte_count {
            len |= (*aml.get(*cur + 1 + i)? as usize) << (4 + 8 * i);
        }
        *cur += 1 + byte_count;
        Some(len)
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
        let p = i + 4;
        let Some(&pkg_op) = aml.get(p) else {
            continue;
        };
        if pkg_op != 0x12 && pkg_op != 0x13 {
            continue;
        }
        let pkg_len_start = p + 1;
        let mut cur = pkg_len_start;
        let Some(pkg_len) = decode_pkg_length(aml, &mut cur) else {
            continue;
        };
        let Some(pkg_end) = pkg_len_start.checked_add(pkg_len) else {
            continue;
        };
        if pkg_end > aml.len() || cur >= pkg_end {
            continue;
        }
        let pkg = &aml[..pkg_end];
        let num_elements = if pkg_op == 0x12 {
            let n = *pkg.get(cur)?;
            cur += 1;
            n as u16
        } else {
            let Some(n) = decode_aml_int(pkg, &mut cur) else {
                continue;
            };
            n
        };
        if num_elements == 0 || cur >= pkg_end {
            continue;
        }
        if let Some(slp_typa) = decode_aml_int(pkg, &mut cur) {
            let slp_typb = if num_elements >= 2 && cur < pkg_end {
                decode_aml_int(pkg, &mut cur).unwrap_or(slp_typa)
            } else {
                slp_typa
            };
            return Some((slp_typa & 0x07, slp_typb & 0x07));
        }
    }
    None
}

fn scan_aml_table_for_s5<H: Hal>(
    hal: &H,
    table_phys: u64,
    expected_sig: &[u8; 4],
) -> Option<(u16, u16)> {
    if table_phys == 0 {
        return None;
    }
    hal.map_mmio(table_phys, 4096);
    let mut sig = [0u8; 4];
    read_bytes(hal, table_phys, &mut sig);
    if &sig != expected_sig {
        return None;
    }
    let table_len = read_u32_le(hal, table_phys + 4) as usize;
    if !(36..=1024 * 1024).contains(&table_len) {
        return None;
    }
    hal.map_mmio(table_phys, table_len);
    if !verify_table_checksum(hal, table_phys, table_len) {
        return None;
    }
    let aml_len = table_len - 36;
    let mut aml = vec![0u8; aml_len];
    read_bytes(hal, table_phys + 36, &mut aml);
    parse_s5_from_aml(&aml)
}

impl AcpiInfo {
    /// Parses the ACPI RSDP at `rsdp_phys`, walks XSDT (for revision >= 2 when `xsdt_address != 0`)
    /// or RSDT, finds the `FACP` (FADT) table, extracts `PM1a_CNT_BLK`, `PM1b_CNT_BLK`, `RESET_REG` /
    /// `RESET_VALUE` (if `FLAGS` bit 10 `RESET_REG_SUP` is set), and scans DSDT (then SSDT) AML
    /// bytecode for the `_S5_` sleep package to determine `SLP_TYPa` and `SLP_TYPb`.
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
            let rsdp_len = u32::from_le_bytes(rsdp[20..24].try_into().ok()?) as usize;
            if (36..=4096).contains(&rsdp_len) && verify_table_checksum(hal, rsdp_phys, rsdp_len) {
                u64::from_le_bytes(rsdp[24..32].try_into().ok()?)
            } else {
                0
            }
        } else {
            0
        };

        let sdt_entries = if revision >= 2 && xsdt_phys != 0 {
            collect_sdt_entries(hal, xsdt_phys, b"XSDT", 8)
        } else {
            None
        }
        .or_else(|| collect_sdt_entries(hal, rsdt_phys, b"RSDT", 4))?;

        let fadt_phys = find_table_in_entries(hal, &sdt_entries, b"FACP")?;

        let fadt_len = read_u32_le(hal, fadt_phys + 4) as usize;
        if !(72..=65536).contains(&fadt_len) {
            return None;
        }
        hal.map_mmio(fadt_phys, fadt_len);
        if !verify_table_checksum(hal, fadt_phys, fadt_len) {
            return None;
        }
        let fadt_rev = hal.mmio_read8(fadt_phys + 8);

        let smi_cmd = u16::try_from(read_u32_le(hal, fadt_phys + 48)).unwrap_or(0);
        let acpi_enable = hal.mmio_read8(fadt_phys + 52);

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
            if reset_addr != 0 && (flags & (1 << 10)) != 0 {
                reset_value = val;
                if space_id == 1 {
                    reset_io_port = u16::try_from(reset_addr).ok();
                } else if space_id == 0 {
                    reset_mmio_addr = Some(reset_addr);
                }
            }
        }

        let s5 = scan_aml_table_for_s5(hal, dsdt_phys, b"DSDT").or_else(|| {
            sdt_entries
                .iter()
                .find_map(|&entry| scan_aml_table_for_s5(hal, entry, b"SSDT"))
        });
        let (slp_typa, slp_typb) = s5?;

        Some(Self {
            pm1a_cnt_blk,
            pm1b_cnt_blk,
            slp_typa,
            slp_typb,
            reset_io_port,
            reset_mmio_addr,
            reset_value,
            smi_cmd,
            acpi_enable,
        })
    }

    /// Commands an ACPI S5 soft-off by writing `(slp_typa << 10) | (1 << 13)` to `pm1a_cnt_blk`
    /// (and `slp_typb` to `pm1b_cnt_blk` if present), preserving non-SLP bits.
    pub fn shutdown<H: Hal>(&self, hal: &H) {
        let mut cur_a = hal.io_read16(self.pm1a_cnt_blk);
        if (cur_a & 1) == 0 && self.smi_cmd != 0 && self.acpi_enable != 0 {
            hal.io_write8(self.smi_cmd, self.acpi_enable);
            for _ in 0..10_000 {
                cur_a = hal.io_read16(self.pm1a_cnt_blk);
                if (cur_a & 1) != 0 {
                    break;
                }
            }
        }
        let val_a = (cur_a & !(0x07 << 10)) | ((self.slp_typa & 0x07) << 10) | (1 << 13);
        hal.io_write16(self.pm1a_cnt_blk, val_a);
        if let Some(pm1b) = self.pm1b_cnt_blk {
            let cur_b = hal.io_read16(pm1b);
            let val_b = (cur_b & !(0x07 << 10)) | ((self.slp_typb & 0x07) << 10) | (1 << 13);
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
