use alloc::vec::Vec;

use super::{le16, le32, le64};
use crate::os::disk::DiskIo;
use crate::os::error::Result;
use crate::store::ChunkInfo;

pub async fn get_ntfs_chunks<D: DiskIo + ?Sized>(
    disk: &mut D,
    start: u64,
    end: u64,
) -> Result<Option<Vec<ChunkInfo>>> {
    if end.saturating_sub(start) < 512 {
        return Ok(None);
    }

    let mut boot_sector = [0u8; 512];
    disk.read(start, &mut boot_sector).await?;

    if &boot_sector[3..11] != b"NTFS    " {
        return Ok(None);
    }

    let bytes_per_sector = le16(&boot_sector, 0x0b) as usize;
    if !(512..=65536).contains(&bytes_per_sector) || !bytes_per_sector.is_power_of_two() {
        return Ok(None);
    }

    let sectors_per_cluster = match boot_sector[0x0d] {
        0 => return Ok(None),
        x @ 1..=127 => x as usize,
        x @ 225..=255 => 1usize << (-(x as i8) as u32),
        _ => return Ok(None),
    };
    let Some(bytes_per_cluster) = bytes_per_sector.checked_mul(sectors_per_cluster) else {
        return Ok(None);
    };
    if bytes_per_cluster == 0 {
        return Ok(None);
    }
    let num_clusters = ((end - start) / bytes_per_cluster as u64) as usize;

    let bytes_per_file_record = match boot_sector[0x40] {
        0 => return Ok(None),
        x @ 1..=127 => match (x as usize).checked_mul(bytes_per_cluster) {
            Some(v) => v,
            None => return Ok(None),
        },
        x @ 225..=255 => 1usize << (-(x as i8) as u32),
        _ => return Ok(None),
    };
    if !(512..=65536).contains(&bytes_per_file_record) || !bytes_per_file_record.is_multiple_of(512)
    {
        return Ok(None);
    }

    let mft_cluster_number = le64(&boot_sector, 0x30);
    let Some(mft_address) = mft_cluster_number.checked_mul(bytes_per_cluster as u64) else {
        return Ok(None);
    };
    let Some(bitmap_entry_address) = mft_address.checked_add(6 * bytes_per_file_record as u64)
    else {
        return Ok(None);
    };
    let Some(record_start) = start.checked_add(bitmap_entry_address) else {
        return Ok(None);
    };
    if record_start.saturating_add(bytes_per_file_record as u64) > end {
        return Ok(None);
    }

    let mut bitmap_entry = vec![0u8; bytes_per_file_record];
    disk.read(record_start, &mut bitmap_entry).await?;

    if &bitmap_entry[0..4] != b"FILE" {
        return Ok(None);
    }

    // Apply NTFS Update Sequence Array (USA) multi-sector fixups across 512-byte sectors.
    let usa_ofs = le16(&bitmap_entry, 0x04) as usize;
    let usa_count = le16(&bitmap_entry, 0x06) as usize;
    if usa_count > 0 {
        let sectors = bytes_per_file_record / 512;
        if usa_count != sectors + 1
            || usa_ofs < 0x18
            || usa_ofs + usa_count * 2 > bytes_per_file_record
        {
            return Ok(None);
        }
        let usn = [bitmap_entry[usa_ofs], bitmap_entry[usa_ofs + 1]];
        for s in 0..sectors {
            let tail = (s + 1) * 512 - 2;
            if bitmap_entry[tail..tail + 2] != usn {
                return Ok(None);
            }
            let fix = usa_ofs + (s + 1) * 2;
            let orig = [bitmap_entry[fix], bitmap_entry[fix + 1]];
            bitmap_entry[tail..tail + 2].copy_from_slice(&orig);
        }
    } else if usa_ofs != 0 {
        return Ok(None);
    }

    let mut attribute_offset = le16(&bitmap_entry, 0x14) as usize;
    let attr_end = loop {
        if attribute_offset + 8 > bitmap_entry.len() {
            return Ok(None);
        }
        let attr_type = le32(&bitmap_entry, attribute_offset);
        if attr_type == 0xFFFF_FFFF {
            return Ok(None);
        }
        let attr_len = le32(&bitmap_entry, attribute_offset + 4) as usize;
        if attr_len < 8 || attribute_offset + attr_len > bitmap_entry.len() {
            return Ok(None);
        }
        if attr_type == 0x80 {
            break attribute_offset + attr_len;
        }
        attribute_offset += attr_len;
    };

    if attribute_offset + 0x22 > attr_end {
        return Ok(None);
    }
    let non_resident_flag = bitmap_entry[attribute_offset + 8];
    if non_resident_flag != 1 {
        return Ok(None);
    }

    let mut start_vcn = le64(&bitmap_entry, attribute_offset + 0x10);
    let last_vcn = le64(&bitmap_entry, attribute_offset + 0x18);
    let run_rel = le16(&bitmap_entry, attribute_offset + 0x20) as usize;
    let mut data_run_offset = match attribute_offset.checked_add(run_rel) {
        Some(off) if off < attr_end => off,
        _ => return Ok(None),
    };

    let mut cur_lcn: u64 = 0;
    let mut cnt: usize = 0;
    let mut chunks = Vec::new();
    let mut buf = vec![0u8; bytes_per_cluster];

    while start_vcn <= last_vcn {
        if data_run_offset >= attr_end {
            return Ok(None);
        }
        let ctrl_byte = bitmap_entry[data_run_offset];
        if ctrl_byte == 0 {
            break;
        }

        let length_len = (ctrl_byte & 0x0f) as usize;
        let offset_len = (ctrl_byte >> 4) as usize;
        if !(1..=7).contains(&length_len) || !(1..=7).contains(&offset_len) {
            return Ok(None);
        }
        let run_end = data_run_offset + 1 + length_len + offset_len;
        if run_end > attr_end {
            return Ok(None);
        }

        let mut length: u64 = 0;
        for i in 0..length_len {
            length |= (bitmap_entry[data_run_offset + 1 + i] as u64) << (8 * i);
        }
        if length == 0 {
            return Ok(None);
        }

        let mut raw_delta: u64 = 0;
        let off_base = data_run_offset + 1 + length_len;
        for i in 0..offset_len {
            raw_delta |= (bitmap_entry[off_base + i] as u64) << (8 * i);
        }
        let shift = 64 - 8 * offset_len;
        let delta = ((raw_delta << shift) as i64) >> shift;
        cur_lcn = match cur_lcn.checked_add_signed(delta) {
            Some(lcn) => lcn,
            None => return Ok(None),
        };

        for i in 0..length {
            let Some(lcn) = cur_lcn.checked_add(i) else {
                return Ok(None);
            };
            let Some(rel_byte) = lcn.checked_mul(bytes_per_cluster as u64) else {
                return Ok(None);
            };
            let Some(x) = start.checked_add(rel_byte) else {
                return Ok(None);
            };
            if x.saturating_add(bytes_per_cluster as u64) > end {
                return Ok(None);
            }
            disk.read(x, &mut buf).await?;

            for &byte in &buf {
                for bit in 0..8 {
                    if cnt < num_clusters {
                        if (byte >> bit) & 1 != 0 {
                            chunks.push(ChunkInfo {
                                start: cnt * bytes_per_cluster,
                                size: bytes_per_cluster,
                            });
                        }
                        cnt += 1;
                    }
                }
            }
        }

        start_vcn = match start_vcn.checked_add(length) {
            Some(v) => v,
            None => return Ok(None),
        };
        data_run_offset = run_end;
    }

    Ok(Some(chunks))
}
