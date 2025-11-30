use super::{error::Result, UefiOS};
use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use gpt_disk_io::{
    gpt_disk_types::{BlockSize, Lba},
    BlockIo,
};
use uefi::{
    boot::{OpenProtocolParams, ScopedProtocol},
    proto::media::block::BlockIO,
    Handle,
};

fn open_disk(handle: Handle) -> Result<ScopedProtocol<BlockIO>> {
    let image_handle = uefi::boot::image_handle();
    let bio = unsafe {
        uefi::boot::open_protocol::<BlockIO>(
            OpenProtocolParams {
                agent: image_handle,
                controller: None,
                handle,
            },
            uefi::boot::OpenProtocolAttributes::GetProtocol,
        )?
    };
    Ok(bio)
}

#[derive(Debug)]
pub struct DiskPartition {
    pub byte_start: u64,
    pub byte_end: u64,
    pub guid: String,
    pub name: String,
}

pub struct Disk {
    block: ScopedProtocol<BlockIO>,
    os: UefiOS,
}

// TODO(veluca): consider making parts of this actually async, i.e. by using DiskIo2/BlockIO2 if
// available; support having more than one disk.
impl Disk {
    pub fn new(os: UefiOS) -> Disk {
        let (_size, handle) = uefi::boot::find_handles::<BlockIO>()
            .unwrap()
            .into_iter()
            .filter_map(|handle| {
                let Ok(block) = open_disk(handle) else {
                    return None;
                };
                let m = block.media();
                if !m.is_media_present() {
                    return None;
                }
                let size = (m.last_block() as u128 + 1) * (m.block_size() as u128);
                Some((size, handle))
            })
            .max_by_key(|(size, _)| *size)
            .expect("Disk not found");

        let block = open_disk(handle).unwrap();
        Disk { block, os }
    }

    #[cfg(feature = "coverage")]
    pub fn open_with_size(os: UefiOS, base_size: i64) -> Disk {
        let (_size, handle) = uefi::boot::find_handles::<BlockIO>()
            .unwrap()
            .into_iter()
            .filter_map(|handle| {
                let Ok(block) = open_disk(handle) else {
                    return None;
                };
                let m = block.media();
                if !m.is_media_present() {
                    return None;
                }
                let size = (m.last_block() as i64 + 1) * (m.block_size() as i64);
                Some(((size - base_size).abs(), handle))
            })
            .min_by_key(|(size, _)| *size)
            .expect("Disk not found");

        let block = open_disk(handle).unwrap();
        Disk { block, os }
    }

    pub fn size(&self) -> u64 {
        self.block.media().block_size() as u64 * (self.block.media().last_block() + 1)
    }

    pub async fn flush(&mut self) -> Result<()> {
        self.block.flush_blocks()?;
        Ok(())
    }

    pub fn read_sync(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let block_size = self.block.media().block_size() as u64;
        let media_id = self.block.media().media_id();
        let start_block = offset / block_size;
        let end_block = (offset + buf.len() as u64).div_ceil(block_size);
        let num_blocks = end_block - start_block;
        if buf.len() as u64 != num_blocks * block_size
            || !(buf.as_ptr() as usize).is_multiple_of(16)
        {
            //log::warn!(
            //    "Unaligned read: offset {}, block size {}, buf addr {:p}, buf len {}",
            //    offset,
            //    block_size,
            //    buf.as_ptr(),
            //    buf.len()
            //);
            let mut buf2 = vec![0u8; (num_blocks * block_size) as usize + 15];
            let delta = buf2.as_ptr().align_offset(16);
            let buf2 = &mut buf2[delta..delta + (num_blocks * block_size) as usize];
            self.block.read_blocks(media_id, start_block, buf2)?;
            let start_offset = (offset % block_size) as usize;
            buf.copy_from_slice(&buf2[start_offset..start_offset + buf.len()]);
        } else {
            self.block.read_blocks(media_id, start_block, buf)?;
        }
        Ok(())
    }

    pub async fn read(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.os.schedule().await;
        self.read_sync(offset, buf)
    }

    pub fn write_sync(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        let block_size = self.block.media().block_size() as u64;
        let media_id = self.block.media().media_id();
        let start_block = offset / block_size;
        let end_block = (offset + buf.len() as u64).div_ceil(block_size);
        let num_blocks = end_block - start_block;
        if buf.len() as u64 != num_blocks * block_size
            || !(buf.as_ptr() as usize).is_multiple_of(16)
        {
            //log::warn!(
            //    "Unaligned write: offset {}, block size {}, buf addr {:p}, buf len {}",
            //    offset,
            //    block_size,
            //    buf.as_ptr(),
            //    buf.len()
            //);
            let mut buf2 = vec![0u8; (num_blocks * block_size) as usize + 15];
            let delta = buf2.as_ptr().align_offset(16);
            let buf2 = &mut buf2[delta..delta + (num_blocks * block_size) as usize];
            self.block.read_blocks(media_id, start_block, buf2)?;
            let start_offset = (offset % block_size) as usize;
            buf2[start_offset..start_offset + buf.len()].copy_from_slice(buf);
            self.block.write_blocks(media_id, start_block, buf2)?;
        } else {
            self.block.write_blocks(media_id, start_block, buf)?;
        }
        Ok(())
    }

    pub async fn write(&mut self, offset: u64, buf: &[u8]) -> Result<()> {
        self.os.schedule().await;
        self.write_sync(offset, buf)
    }

    pub fn partitions(&mut self) -> Result<Vec<DiskPartition>> {
        let block_size = self.block_size().to_u64();
        let mut disk = gpt_disk_io::Disk::new(self)?;
        let mut buf = [0; 1 << 14];
        let header = disk.read_primary_gpt_header(&mut buf)?;
        // TODO(veluca): bubble up this error.
        let part_array_layout = header.get_partition_entry_array_layout()?;
        let mut buf = [0; 1 << 14];
        let x = disk
            .gpt_partition_entry_array_iter(part_array_layout, &mut buf)?
            .filter_map(|part| {
                let part = if let Err(err) = part {
                    return Some(Err(err));
                } else {
                    part.unwrap()
                };
                if part.is_used() {
                    let part_guid = part.unique_partition_guid;
                    Some(Ok(DiskPartition {
                        byte_start: part.starting_lba.to_u64() * block_size,
                        byte_end: (part.ending_lba.to_u64() + 1) * block_size,
                        guid: part_guid.to_string(),
                        name: part.name.to_string(),
                    }))
                } else {
                    None
                }
            })
            .collect::<Result<_, _>>()?;

        Ok(x)
    }
}

impl gpt_disk_io::BlockIo for &mut Disk {
    type Error = super::error::Error;

    fn block_size(&self) -> BlockSize {
        BlockSize::new(self.block.media().block_size()).unwrap()
    }
    fn num_blocks(&mut self) -> Result<u64> {
        Ok(self.block.media().last_block() + 1)
    }
    fn read_blocks(&mut self, start_lba: Lba, dst: &mut [u8]) -> Result<()> {
        self.read_sync(self.block.media().block_size() as u64 * start_lba.0, dst)
    }
    fn write_blocks(&mut self, _start_lba: Lba, _src: &[u8]) -> Result<()> {
        unreachable!();
    }
    fn flush(&mut self) -> Result<()> {
        // This is a no-op because write_blocks isn't implemented.
        Ok(())
    }
}
