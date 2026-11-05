use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use pixie_client::parse_disk::{ext4, fat, ntfs, parse_disk, swap};
use pixie_client::store::ChunkInfo;
use pixie_kernel::disk::{DiskIo, read_gpt_partitions};
use pixie_kernel::error::{Error, Result};

fn block_on<F: Future>(mut fut: F) -> F::Output {
    fn raw_waker() -> RawWaker {
        fn no_op(_: *const ()) {}
        fn clone(_: *const ()) -> RawWaker {
            raw_waker()
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, no_op, no_op, no_op);
        RawWaker::new(core::ptr::null(), &VTABLE)
    }

    let waker = unsafe { Waker::from_raw(raw_waker()) };
    let mut cx = Context::from_waker(&waker);
    let mut fut = unsafe { Pin::new_unchecked(&mut fut) };

    for _ in 0..1_000_000 {
        if let Poll::Ready(output) = fut.as_mut().poll(&mut cx) {
            return output;
        }
    }
    panic!("Future did not resolve within 1,000,000 poll iterations");
}

struct MemDisk {
    data: Vec<u8>,
}

impl MemDisk {
    fn new(size_bytes: usize) -> Self {
        Self {
            data: vec![0u8; size_bytes],
        }
    }
}

impl DiskIo for MemDisk {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }

    fn block_size(&self) -> u32 {
        512
    }

    async fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let start = offset as usize;
        let end = start + buf.len();
        if end > self.data.len() {
            return Err(Error::msg("MemDisk read out of bounds"));
        }
        buf.copy_from_slice(&self.data[start..end]);
        Ok(())
    }

    async fn write(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        let start = offset as usize;
        let end = start + buf.len();
        if end > self.data.len() {
            return Err(Error::msg("MemDisk write out of bounds"));
        }
        self.data[start..end].copy_from_slice(buf);
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

fn crc32_ieee(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            if (crc & 1) != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

fn write_swap_partition(buf: &mut [u8]) {
    assert!(buf.len() >= 4096);
    buf[4096 - 10..4096].copy_from_slice(b"SWAPSPACE2");
}

fn write_fat16_partition(buf: &mut [u8]) {
    // 512B sectors, 8 sectors/cluster (4 KiB), 4 reserved sectors, 2 FATs of 4 sectors each,
    // 32 root dir entries (2 sectors) -> first_data_sector = 4 + 2*4 + 2 = 14 (byte 7168).
    let total_sectors = (buf.len() / 512) as u16;
    buf[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes()); // bytes per sector
    buf[0x0D] = 8; // sectors per cluster -> 4096B
    buf[0x0E..0x10].copy_from_slice(&4u16.to_le_bytes()); // reserved sectors = 4
    buf[0x10] = 2; // 2 FATs
    buf[0x11..0x13].copy_from_slice(&32u16.to_le_bytes()); // 32 root dir entries -> 2 sectors
    buf[0x13..0x15].copy_from_slice(&total_sectors.to_le_bytes());
    buf[0x16..0x18].copy_from_slice(&4u16.to_le_bytes()); // 4 sectors per FAT
    buf[0x36..0x3E].copy_from_slice(b"FAT16   ");
    buf[510] = 0x55;
    buf[511] = 0xAA;

    // FAT1 starts at sector 4 (offset 2048): mark cluster 2 (idx=2) and cluster 4 (idx=4) allocated
    let fat_off = 4 * 512;
    buf[fat_off..fat_off + 2].copy_from_slice(&0xFFF8u16.to_le_bytes());
    buf[fat_off + 2..fat_off + 4].copy_from_slice(&0xFFFFu16.to_le_bytes());
    buf[fat_off + 4..fat_off + 6].copy_from_slice(&0xFFFFu16.to_le_bytes()); // cluster 2 used
    buf[fat_off + 6..fat_off + 8].copy_from_slice(&0x0000u16.to_le_bytes()); // cluster 3 free
    buf[fat_off + 8..fat_off + 10].copy_from_slice(&0xFFFFu16.to_le_bytes()); // cluster 4 used
}

fn write_ext4_partition(buf: &mut [u8]) {
    // 4 KiB blocks (log_block_size = 2), 64 blocks total (256 KiB), 1 group, 64-bit group descriptor
    let block_size = 4096usize;
    let total_blocks = (buf.len() / block_size) as u32;
    let sb = &mut buf[1024..2048];
    sb[0x04..0x08].copy_from_slice(&total_blocks.to_le_bytes()); // s_blocks_count_lo
    sb[0x14..0x18].copy_from_slice(&0u32.to_le_bytes()); // s_first_data_block = 0
    sb[0x18..0x1C].copy_from_slice(&2u32.to_le_bytes()); // s_log_block_size = 2 (4096B)
    sb[0x20..0x24].copy_from_slice(&32768u32.to_le_bytes()); // s_blocks_per_group
    sb[0x38..0x3A].copy_from_slice(&0xEF53u16.to_le_bytes()); // s_magic = 0xEF53
    sb[0x60..0x64].copy_from_slice(&0x80u32.to_le_bytes()); // s_feature_incompat = INCOMPAT_64BIT
    sb[0x64..0x68].copy_from_slice(&0x01u32.to_le_bytes()); // s_feature_ro_compat = RO_COMPAT_SPARSE_SUPER
    sb[0xFE..0x100].copy_from_slice(&64u16.to_le_bytes()); // s_desc_size = 64

    // Group 0 descriptor at block 1 (offset 4096): block bitmap at block 2
    let gd = &mut buf[4096..4096 + 64];
    gd[0x00..0x04].copy_from_slice(&2u32.to_le_bytes()); // bg_block_bitmap_lo = 2
    gd[0x20..0x24].copy_from_slice(&0u32.to_le_bytes()); // bg_block_bitmap_hi = 0
    gd[0x12..0x14].copy_from_slice(&0u16.to_le_bytes()); // bg_flags = 0

    // Block bitmap at block 2 (offset 8192): mark blocks 0, 1, 2, and 5 allocated (0b0010_0111 = 0x27)
    buf[8192] = 0b0010_0111;
}

fn write_ntfs_partition(buf: &mut [u8]) {
    // 512B sectors, 8 sectors/cluster (4 KiB), MFT at cluster 2 (offset 8192), 1024B MFT records (-10 = 0xF6)
    buf[3..11].copy_from_slice(b"NTFS    ");
    buf[0x0B..0x0D].copy_from_slice(&512u16.to_le_bytes()); // bytes_per_sector = 512
    buf[0x0D] = 8; // sectors_per_cluster = 8 (4096B)
    buf[0x30..0x38].copy_from_slice(&2u64.to_le_bytes()); // mft_cluster_number = 2 (offset 8192)
    buf[0x40] = (-10i8) as u8; // bytes_per_file_record = 2^10 = 1024B

    // MFT Record 6 ($Bitmap) at offset 8192 + 6 * 1024 = 14336
    let mft6 = &mut buf[14336..14336 + 1024];
    mft6[0..4].copy_from_slice(b"FILE");
    mft6[0x14..0x16].copy_from_slice(&56u16.to_le_bytes()); // first attribute at offset 56

    // Attribute 0x80 ($DATA) at offset 56
    let attr = &mut mft6[56..];
    attr[0..4].copy_from_slice(&0x80u32.to_le_bytes()); // type = 0x80 ($DATA)
    attr[4..8].copy_from_slice(&72u32.to_le_bytes()); // length = 72
    attr[8] = 1; // non-resident = 1
    attr[0x10..0x18].copy_from_slice(&0u64.to_le_bytes()); // start_vcn = 0
    attr[0x18..0x20].copy_from_slice(&0u64.to_le_bytes()); // last_vcn = 0
    attr[0x20..0x22].copy_from_slice(&64u16.to_le_bytes()); // data_run_offset = 64
    // Data run at attr[64..]: 0x11 -> 1-byte length (1 cluster), 1-byte LCN offset (cluster 6 -> offset 24576)
    attr[64] = 0x11;
    attr[65] = 1;
    attr[66] = 6;
    attr[67] = 0;

    // Cluster 6 (offset 6 * 4096 = 24576) holds the cluster allocation bitmap:
    // Mark clusters 0, 2, and 6 as allocated -> byte 0 = (1 << 0) | (1 << 2) | (1 << 6) = 0x45
    buf[6 * 4096] = 0b0100_0101;
}

#[test]
fn test_parse_swap_partition_async() {
    let mut disk = MemDisk::new(64 * 1024);
    write_swap_partition(&mut disk.data);

    let chunks = block_on(swap::get_swap_chunks(&mut disk, 0, 64 * 1024))
        .expect("get_swap_chunks should succeed")
        .expect("Should detect SWAPSPACE2 signature");

    // Only the first 4096-byte page (header page) should be marked in-use!
    assert_eq!(
        chunks,
        vec![ChunkInfo {
            start: 0,
            size: 4096
        }]
    );
}

#[test]
fn test_parse_fat16_partition_async() {
    let mut disk = MemDisk::new(128 * 1024);
    write_fat16_partition(&mut disk.data);

    let chunks = block_on(fat::get_fat_chunks(&mut disk, 0, 128 * 1024))
        .expect("get_fat_chunks should succeed")
        .expect("Should detect FAT16 partition");

    // Metadata region: first_data_sector = 14 sectors = 7168 bytes
    // Cluster 2: 7168 .. 7168 + 4096
    // Cluster 4: 7168 + 2 * 4096 .. 7168 + 3 * 4096 (Cluster 3 is free!)
    assert_eq!(
        chunks,
        vec![
            ChunkInfo {
                start: 0,
                size: 7168
            },
            ChunkInfo {
                start: 7168,
                size: 4096,
            },
            ChunkInfo {
                start: 7168 + 2 * 4096,
                size: 4096,
            },
        ]
    );
}

#[test]
fn test_parse_ext4_partition_async() {
    let mut disk = MemDisk::new(256 * 1024);
    write_ext4_partition(&mut disk.data);

    let chunks = block_on(ext4::get_ext4_chunks(&mut disk, 0, 256 * 1024))
        .expect("get_ext4_chunks should succeed")
        .expect("Should detect 64-bit ext4 partition");

    // Blocks 0, 1, 2, and 5 marked in bitmap -> 4 chunks of 4096B (or merged by parse_partition)
    assert_eq!(
        chunks,
        vec![
            ChunkInfo {
                start: 0,
                size: 4096
            },
            ChunkInfo {
                start: 4096,
                size: 4096,
            },
            ChunkInfo {
                start: 8192,
                size: 4096,
            },
            ChunkInfo {
                start: 5 * 4096,
                size: 4096,
            },
        ]
    );

    // Also verify parse_disk merges contiguous chunks (0, 1, 2 -> 0..12288)
    let merged = block_on(parse_disk(&mut disk)).expect("parse_disk on ext4 should succeed");
    assert_eq!(
        merged,
        vec![
            ChunkInfo {
                start: 0,
                size: 12288
            },
            ChunkInfo {
                start: 5 * 4096,
                size: 4096,
            },
        ]
    );
}

#[test]
fn test_parse_ntfs_partition_async() {
    let mut disk = MemDisk::new(128 * 1024);
    write_ntfs_partition(&mut disk.data);

    let chunks = block_on(ntfs::get_ntfs_chunks(&mut disk, 0, 128 * 1024))
        .expect("get_ntfs_chunks should succeed")
        .expect("Should detect NTFS partition");

    // Clusters 0, 2, and 6 marked allocated in $Bitmap
    assert_eq!(
        chunks,
        vec![
            ChunkInfo {
                start: 0,
                size: 4096
            },
            ChunkInfo {
                start: 2 * 4096,
                size: 4096,
            },
            ChunkInfo {
                start: 6 * 4096,
                size: 4096,
            },
        ]
    );
}

#[test]
fn test_read_gpt_partitions_and_full_parse_disk_async() {
    // Construct a 1 MiB (2048-sector) GPT disk with two partitions:
    // - Partition 1 (LBA 64..191, 64 KiB): Linux Swap v1
    // - Partition 2 (LBA 192..703, 256 KiB): ext4
    let total_lbas = 2048u64;
    let mut disk = MemDisk::new((total_lbas as usize) * 512);

    // 1. Protective MBR at LBA 0
    disk.data[446 + 4] = 0xEE; // GPT Protective partition type
    disk.data[446 + 8..446 + 12].copy_from_slice(&1u32.to_le_bytes());
    disk.data[446 + 12..446 + 16].copy_from_slice(&((total_lbas - 1) as u32).to_le_bytes());
    disk.data[510] = 0x55;
    disk.data[511] = 0xAA;

    // 2. Build 4 partition entries (512 bytes = 1 sector of entries for compactness, or 128 entries = 32 sectors)
    let num_entries = 128u32;
    let entry_size = 128u32;
    let entries_bytes = (num_entries * entry_size) as usize; // 16384 bytes = 32 sectors
    let mut entries = vec![0u8; entries_bytes];

    // Entry 0: Swap partition (LBA 64..191)
    entries[0..16].copy_from_slice(&[0x01; 16]); // type GUID (non-zero)
    entries[16..32].copy_from_slice(&[0x11; 16]); // unique GUID
    entries[32..40].copy_from_slice(&64u64.to_le_bytes()); // starting_lba = 64
    entries[40..48].copy_from_slice(&191u64.to_le_bytes()); // ending_lba = 191
    for (i, ch) in "swap-part".encode_utf16().enumerate() {
        entries[56 + i * 2..56 + i * 2 + 2].copy_from_slice(&ch.to_le_bytes());
    }

    // Entry 1: ext4 partition (LBA 192..703)
    let e1 = &mut entries[128..256];
    e1[0..16].copy_from_slice(&[0x02; 16]); // type GUID
    e1[16..32].copy_from_slice(&[0x22; 16]); // unique GUID
    e1[32..40].copy_from_slice(&192u64.to_le_bytes()); // starting_lba = 192
    e1[40..48].copy_from_slice(&703u64.to_le_bytes()); // ending_lba = 703
    for (i, ch) in "ext4-part".encode_utf16().enumerate() {
        e1[56 + i * 2..56 + i * 2 + 2].copy_from_slice(&ch.to_le_bytes());
    }

    let entries_crc = crc32_ieee(&entries);

    // Primary entries at LBA 2..34, Secondary entries at LBA (2047 - 32)..2047 = 2015..2047
    let primary_entries_lba = 2u64;
    let secondary_entries_lba = total_lbas - 1 - 32;
    disk.data[(primary_entries_lba as usize) * 512
        ..(primary_entries_lba as usize) * 512 + entries_bytes]
        .copy_from_slice(&entries);
    disk.data[(secondary_entries_lba as usize) * 512
        ..(secondary_entries_lba as usize) * 512 + entries_bytes]
        .copy_from_slice(&entries);

    // Helper to build a 92-byte GPT header at `my_lba`
    let build_gpt_header = |my_lba: u64, alt_lba: u64, part_entry_lba: u64| -> [u8; 512] {
        let mut hdr = [0u8; 512];
        hdr[0..8].copy_from_slice(b"EFI PART");
        hdr[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes()); // revision 1.0
        hdr[12..16].copy_from_slice(&92u32.to_le_bytes()); // header_size = 92
        hdr[24..32].copy_from_slice(&my_lba.to_le_bytes());
        hdr[32..40].copy_from_slice(&alt_lba.to_le_bytes());
        hdr[40..48].copy_from_slice(&34u64.to_le_bytes()); // first_usable_lba = 34
        hdr[48..56].copy_from_slice(&(secondary_entries_lba - 1).to_le_bytes()); // last_usable_lba
        hdr[56..72].copy_from_slice(&[0xAB; 16]); // disk_guid
        hdr[72..80].copy_from_slice(&part_entry_lba.to_le_bytes());
        hdr[80..84].copy_from_slice(&num_entries.to_le_bytes());
        hdr[84..88].copy_from_slice(&entry_size.to_le_bytes());
        hdr[88..92].copy_from_slice(&entries_crc.to_le_bytes());
        let hdr_crc = crc32_ieee(&hdr[0..92]);
        hdr[16..20].copy_from_slice(&hdr_crc.to_le_bytes());
        hdr
    };

    let pri_hdr = build_gpt_header(1, total_lbas - 1, primary_entries_lba);
    let sec_hdr = build_gpt_header(total_lbas - 1, 1, secondary_entries_lba);
    disk.data[512..1024].copy_from_slice(&pri_hdr);
    disk.data[((total_lbas - 1) as usize) * 512..].copy_from_slice(&sec_hdr);

    // Populate Partition 1 (Swap at LBA 64..191 -> bytes 32768..98304)
    write_swap_partition(&mut disk.data[64 * 512..192 * 512]);
    // Populate Partition 2 (ext4 at LBA 192..703 -> bytes 98304..360448)
    write_ext4_partition(&mut disk.data[192 * 512..704 * 512]);

    // 1. Verify read_gpt_partitions
    let (primary_range, secondary_range, parts) =
        block_on(read_gpt_partitions(&mut disk)).expect("read_gpt_partitions should succeed");
    assert_eq!(primary_range, (512, 34 * 512));
    assert_eq!(
        secondary_range,
        (secondary_entries_lba * 512, total_lbas * 512)
    );
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0].name, "swap-part");
    assert_eq!(parts[0].byte_start, 64 * 512);
    assert_eq!(parts[0].byte_end, 192 * 512);
    assert_eq!(parts[1].name, "ext4-part");
    assert_eq!(parts[1].byte_start, 192 * 512);
    assert_eq!(parts[1].byte_end, 704 * 512);

    // 2. Verify full parse_disk
    let disk_chunks = block_on(parse_disk(&mut disk)).expect("parse_disk should succeed");
    assert_eq!(
        disk_chunks,
        vec![
            // Primary GPT (MBR + Primary Header + Entries: 0 .. 34 * 512 = 17408)
            ChunkInfo {
                start: 0,
                size: 34 * 512,
            },
            // Partition 1 (Swap header page at 64 * 512 = 32768, size 4096)
            ChunkInfo {
                start: 64 * 512,
                size: 4096,
            },
            // Partition 2 (ext4 blocks 0..3 at 192 * 512 = 98304, size 12288)
            ChunkInfo {
                start: 192 * 512,
                size: 12288,
            },
            // Partition 2 (ext4 block 5 at 192 * 512 + 5 * 4096 = 118784, size 4096)
            ChunkInfo {
                start: 192 * 512 + 5 * 4096,
                size: 4096,
            },
            // Secondary GPT (Entries + Header at 2015 * 512 .. 2048 * 512)
            ChunkInfo {
                start: (secondary_entries_lba as usize) * 512,
                size: 33 * 512,
            },
        ]
    );
}
