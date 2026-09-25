use alloc::vec::Vec;

use super::{le16, le32, le64};
use crate::os::disk::Disk;
use crate::os::error::Result;
use crate::store::ChunkInfo;

pub async fn get_ntfs_chunks(
    disk: &mut Disk,
    start: u64,
    end: u64,
) -> Result<Option<Vec<ChunkInfo>>> {
    if end - start < 512 {
        return Ok(None);
    }

    let mut boot_sector = [0u8; 512];
    disk.read(start, &mut boot_sector).await?;

    if &boot_sector[3..11] != b"NTFS    " {
        return Ok(None);
    }

    let bytes_per_sector = le16(&boot_sector, 0x0b) as usize;

    let sectors_per_cluster = match boot_sector[0x0d] {
        x @ 0..=127 => x as usize,
        x @ 225..=255 => 1 << -(x as i8),
        x @ 128..=224 => panic!("too many sectors per cluster: {}", x),
    };
    let bytes_per_cluster = bytes_per_sector * sectors_per_cluster;
    let num_clusters = (end as usize - start as usize) / bytes_per_cluster;

    let bytes_per_file_record = match boot_sector[0x40] {
        x @ 0..=127 => x as usize * bytes_per_cluster,
        x @ 225..=255 => 1 << -(x as i8),
        x @ 128..=224 => panic!("too many bytes per file record: {}", x),
    };

    let mft_cluster_number = le64(&boot_sector, 0x30) as usize;
    let mft_address = bytes_per_cluster * mft_cluster_number;

    let bitmap_entry_address = mft_address + 6 * bytes_per_file_record;
    let mut bitmap_entry = [0u8; 1024];
    disk.read(start + bitmap_entry_address as u64, &mut bitmap_entry)
        .await?;

    let mut attribute_offset = le16(&bitmap_entry, 0x14) as usize;
    while attribute_offset + 8 <= bitmap_entry.len() {
        let attr_type = le32(&bitmap_entry, attribute_offset);
        if attr_type == 0x80 {
            break;
        }
        if attr_type == 0xFFFF_FFFF {
            return Ok(None);
        }
        let len = le32(&bitmap_entry, attribute_offset + 4) as usize;
        if len == 0 || attribute_offset + len > bitmap_entry.len() {
            return Ok(None);
        }
        attribute_offset += len;
    }
    if attribute_offset + 8 > bitmap_entry.len() {
        return Ok(None);
    }

    let non_resident_flag = bitmap_entry[attribute_offset + 8];
    if non_resident_flag != 1 {
        return Ok(None);
    }

    let mut start_vcn = le64(&bitmap_entry, attribute_offset + 0x10) as usize;
    let last_vcn = le64(&bitmap_entry, attribute_offset + 0x18) as usize;
    let mut data_run_offset =
        attribute_offset + le16(&bitmap_entry, attribute_offset + 0x20) as usize;

    let mut cnt = 0;
    let mut chunks = Vec::new();
    let mut current_lcn: i64 = 0;

    while start_vcn <= last_vcn && data_run_offset < bitmap_entry.len() {
        let ctrl_byte = bitmap_entry[data_run_offset];
        if ctrl_byte == 0 {
            break;
        }

        let length_len = (ctrl_byte & 0x0f) as usize;
        let offset_len = (ctrl_byte >> 4) as usize;
        if length_len == 0
            || length_len > 8
            || offset_len > 8
            || data_run_offset + 1 + length_len + offset_len > bitmap_entry.len()
        {
            break;
        }

        let mut length: u64 = 0;
        for b in 0..length_len {
            length |= (bitmap_entry[data_run_offset + 1 + b] as u64) << (8 * b);
        }

        let mut offset_raw: u64 = 0;
        for b in 0..offset_len {
            offset_raw |= (bitmap_entry[data_run_offset + 1 + length_len + b] as u64) << (8 * b);
        }

        let offset_delta = if offset_len > 0 {
            let shift = 64 - (offset_len * 8);
            ((offset_raw << shift) as i64) >> shift
        } else {
            0
        };
        current_lcn += offset_delta;
        let lcn = current_lcn as u64;

        let mut buf = vec![0u8; bytes_per_cluster];
        for i in 0..length {
            let x = start + (lcn + i) * bytes_per_cluster as u64;
            disk.read(x, &mut buf).await?;

            for &byte in &buf {
                for bit in 0..8 {
                    if cnt < num_clusters as u64 {
                        if byte >> bit & 1 != 0 {
                            ChunkInfo::push(
                                &mut chunks,
                                cnt as usize * bytes_per_cluster,
                                bytes_per_cluster,
                            );
                        }
                        cnt += 1;
                    }
                }
            }
        }

        start_vcn += length as usize;
        data_run_offset += 1 + length_len + offset_len;
    }

    Ok(Some(chunks))
}
