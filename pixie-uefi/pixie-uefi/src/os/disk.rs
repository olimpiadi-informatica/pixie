use uefi::{
    proto::media::{block::BlockIO, disk::DiskIo},
    table::boot::{OpenProtocolParams, ScopedProtocol},
    Handle,
};

use super::{error::Result, UefiOS};

fn open_disk(
    os: UefiOS,
    handle: Handle,
) -> Result<(
    ScopedProtocol<'static, DiskIo>,
    ScopedProtocol<'static, BlockIO>,
)> {
    let bs = os.os().borrow().boot_services;
    let image_handle = os.os().borrow().boot_services.image_handle();
    let bio = unsafe {
        bs.open_protocol::<BlockIO>(
            OpenProtocolParams {
                agent: image_handle,
                controller: None,
                handle,
            },
            uefi::table::boot::OpenProtocolAttributes::GetProtocol,
        )
    };
    Ok((os.open_handle(handle)?, bio?))
}

pub struct Disk {
    os: UefiOS,
    disk: ScopedProtocol<'static, DiskIo>,
    block: ScopedProtocol<'static, BlockIO>,
}

// TODO(veluca): consider making parts of this actually async, i.e. by using DiskIo2/BlockIO2 if
// available; support having more than one disk.
impl Disk {
    pub fn new(os: UefiOS) -> Disk {
        let handle = os
            .all_handles::<DiskIo>()
            .unwrap()
            .into_iter()
            .find(|handle| {
                let op = open_disk(os, *handle);
                if let Ok((_, block)) = op {
                    if block.media().is_media_present() {
                        return true;
                    }
                }
                return false;
            })
            .expect("Disk not found");

        let (disk, block) = open_disk(os, handle).unwrap();
        Disk {
            os: os.clone(),
            disk,
            block,
        }
    }

    pub fn size(&self) -> u64 {
        self.block.media().block_size() as u64 * (self.block.media().last_block() + 1)
    }

    pub async fn flush(&mut self) {
        self.block.flush_blocks().unwrap();
    }
}
