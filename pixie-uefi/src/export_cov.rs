use crate::os::disk::Disk;
use crate::os::UefiOS;

pub async fn export(os: UefiOS) {
    let mut disk = Disk::open_with_size(os, 500 << 20);

    let mut coverage = vec![];
    // SAFETY: we never create threads anyway.
    unsafe { minicov::capture_coverage(&mut coverage).unwrap() };
    disk.write_sync(0, &coverage).unwrap();
}
