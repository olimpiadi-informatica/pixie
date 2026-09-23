#![allow(dead_code)]
use spin::Mutex;

#[derive(Clone, Copy, Debug)]
pub struct BootInfo {
    pub framebuffer_base: u64,
    pub framebuffer_size: usize,
    pub fb_width: u32,
    pub fb_height: u32,
    pub fb_stride: u32,
    pub rsdp_addr: Option<u64>,
    pub tsc_ticks_per_micro: i64,
}

unsafe impl Send for BootInfo {}
unsafe impl Sync for BootInfo {}

pub static BOOT_INFO: Mutex<Option<BootInfo>> = Mutex::new(None);

pub fn set_boot_info(info: BootInfo) {
    *BOOT_INFO.lock() = Some(info);
}

pub fn get_boot_info() -> BootInfo {
    BOOT_INFO.lock().expect("BootInfo not initialized")
}
