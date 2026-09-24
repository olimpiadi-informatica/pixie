use crate::os::disk::Disk;

pub async fn export() {
    let mut disk = Disk::open_with_size(500 << 20).await;

    let mut coverage = vec![];
    // SAFETY: we never create threads anyway.
    let _ = unsafe { minicov::capture_coverage(&mut coverage) };
    if let Err(e) = disk.write_sync(0, &coverage) {
        log::warn!("Failed to export coverage to disk: {e}");
    }
}
