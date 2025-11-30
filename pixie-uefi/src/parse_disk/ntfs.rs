use super::{le16, le32, le64};
use crate::{
    os::{disk::Disk, error::Result},
    store::ChunkInfo,
};
use alloc::vec::Vec;

pub async fn get_ntfs_chunks(disk: &Disk, start: u64, end: u64) -> Result<Option<Vec<ChunkInfo>>> {
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
    let num_clusters = (end as usize - start as usize).div_ceil(bytes_per_cluster);

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
    while le32(&bitmap_entry, attribute_offset) != 0x80 {
        attribute_offset += le32(&bitmap_entry, attribute_offset + 4) as usize;
    }

    let non_resident_flag = bitmap_entry[attribute_offset + 8];
    assert_eq!(non_resident_flag, 1);

    let mut start_vcn = le64(&bitmap_entry, attribute_offset + 0x10) as usize;
    let last_vcn = le64(&bitmap_entry, attribute_offset + 0x18) as usize;
    let mut data_run_offset =
        attribute_offset + le16(&bitmap_entry, attribute_offset + 0x20) as usize;

    let mut cnt = 0;
    let mut chunks = Vec::new();

    while start_vcn <= last_vcn {
        let ctrl_byte = bitmap_entry[data_run_offset];

        let length_len = (ctrl_byte & 0x0f) as usize;
        let length =
            (le64(&bitmap_entry, data_run_offset + 1) & ((1 << (8 * length_len)) - 1)) as usize;

        let offset_len = (ctrl_byte >> 4) as usize;
        let offset = (le64(&bitmap_entry, data_run_offset + 1 + length_len)
            & ((1 << (8 * offset_len)) - 1)) as usize;

        let mut buf = vec![0u8; bytes_per_cluster];
        for i in 0..length {
            let x = start + (offset + i) as u64 * bytes_per_cluster as u64;
            disk.read(x, &mut buf).await?;

            for &byte in &buf {
                for bit in 0..8 {
                    if cnt < num_clusters as u64 {
                        if byte >> bit & 1 != 0 {
                            chunks.push(ChunkInfo {
                                start: cnt as usize * bytes_per_cluster,
                                size: bytes_per_cluster,
                            });
                        }
                        cnt += 1;
                    }
                }
            }
        }

        start_vcn += length;
        data_run_offset += 1 + length_len + offset_len;
    }

    Ok(Some(chunks))
}
