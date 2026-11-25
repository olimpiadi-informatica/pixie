use crate::os::disk::Disk;

pub async fn export() {
    let mut disk = Disk::open_with_size(500 << 20).await;

    let mut coverage = vec![];
    // SAFETY: single-threaded bare-metal execution.
    unsafe { minicov::capture_coverage(&mut coverage).unwrap() };
    disk.write(0, &coverage).await.unwrap();
    disk.flush().await.unwrap();
}
