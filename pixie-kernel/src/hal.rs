use core::future::Future;
use core::time::Duration;

use pixie_drivers::hal::{DmaBuffer, Hal};

use crate::executor::Executor;
use crate::timer::Timer;

#[derive(Debug, Clone, Copy, Default)]
pub struct NativeHal;

impl Hal for NativeHal {
    fn mmio_read8(&self, addr: u64) -> u8 {
        unsafe { core::ptr::read_volatile(addr as *const u8) }
    }
    fn mmio_write8(&self, addr: u64, val: u8) {
        unsafe { core::ptr::write_volatile(addr as *mut u8, val) }
    }
    fn mmio_read16(&self, addr: u64) -> u16 {
        unsafe { core::ptr::read_volatile(addr as *const u16) }
    }
    fn mmio_write16(&self, addr: u64, val: u16) {
        unsafe { core::ptr::write_volatile(addr as *mut u16, val) }
    }
    fn mmio_read32(&self, addr: u64) -> u32 {
        unsafe { core::ptr::read_volatile(addr as *const u32) }
    }
    fn mmio_write32(&self, addr: u64, val: u32) {
        unsafe { core::ptr::write_volatile(addr as *mut u32, val) }
    }
    fn mmio_read64(&self, addr: u64) -> u64 {
        unsafe { core::ptr::read_volatile(addr as *const u64) }
    }
    fn mmio_write64(&self, addr: u64, val: u64) {
        unsafe { core::ptr::write_volatile(addr as *mut u64, val) }
    }

    fn io_read8(&self, port: u16) -> u8 {
        unsafe { crate::arch::io::inb(port) }
    }
    fn io_write8(&self, port: u16, val: u8) {
        unsafe { crate::arch::io::outb(port, val) }
    }
    fn io_read16(&self, port: u16) -> u16 {
        unsafe { crate::arch::io::inw(port) }
    }
    fn io_write16(&self, port: u16, val: u16) {
        unsafe { crate::arch::io::outw(port, val) }
    }
    fn io_read32(&self, port: u16) -> u32 {
        unsafe { crate::arch::io::inl(port) }
    }
    fn io_write32(&self, port: u16, val: u32) {
        unsafe { crate::arch::io::outl(port, val) }
    }

    fn alloc_dma(&self, pages: usize) -> Option<DmaBuffer> {
        let phys = crate::memory::alloc_dma_pages(pages)?;
        let size = pages * pixie_drivers::hal::PAGE_SIZE;
        let virt = phys as *mut u8;
        Some(DmaBuffer { virt, phys, size })
    }

    fn free_dma(&self, buf: DmaBuffer) {
        let pages = buf.size.div_ceil(pixie_drivers::hal::PAGE_SIZE);
        crate::memory::free_contiguous(buf.phys, pages);
    }

    fn map_mmio(&self, phys: u64, size: usize) {
        crate::arch::paging::map_mmio(phys, size);
    }

    fn micros(&self) -> i64 {
        Timer::micros()
    }

    fn yield_now(&self) -> impl Future<Output = ()> {
        Executor::sched_yield()
    }

    fn sleep(&self, duration: Duration) -> impl Future<Output = ()> {
        Executor::sleep(duration)
    }
}
