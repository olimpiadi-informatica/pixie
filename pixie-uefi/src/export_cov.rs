use crate::os::disk::Disk;

pub async fn export() {
    let mut disk = Disk::open_with_size(500 << 20);

    let mut coverage = vec![];
    // SAFETY: we never create threads anyway.
    unsafe { minicov::capture_coverage(&mut coverage).unwrap() };
    disk.write_sync(0, &coverage).unwrap();
}
