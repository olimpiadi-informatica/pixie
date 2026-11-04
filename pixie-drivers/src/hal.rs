use alloc::rc::Rc;
use alloc::sync::Arc;
use core::future::Future;
use core::ptr;
use core::time::Duration;

use crate::error::{Error, Result};

pub const PAGE_SIZE: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmaBuffer {
    pub virt: *mut u8,
    pub phys: u64,
    pub size: usize,
}

unsafe impl Send for DmaBuffer {}
unsafe impl Sync for DmaBuffer {}

impl DmaBuffer {
    /// View the DMA buffer as a shared byte slice.
    ///
    /// # Safety
    /// The caller must ensure no concurrent mutable access or active device DMA write conflicts.
    pub unsafe fn as_slice(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self.virt, self.size) }
    }

    /// View the DMA buffer as a mutable byte slice.
    ///
    /// # Safety
    /// The caller must ensure exclusive access and no concurrent device DMA access.
    pub unsafe fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.virt, self.size) }
    }
}

/// Allocates `N` DMA buffers according to `pages`, rolling back any already-allocated
/// buffers via `hal.free_dma(...)` if an allocation fails.
pub(crate) fn alloc_dma_buffers<H: Hal, const N: usize>(
    hal: &H,
    pages: [usize; N],
    err: &'static str,
) -> Result<[DmaBuffer; N]> {
    let mut bufs = [DmaBuffer {
        virt: ptr::null_mut(),
        phys: 0,
        size: 0,
    }; N];
    for i in 0..N {
        let Some(buf) = hal.alloc_dma(pages[i]) else {
            for b in &bufs[..i] {
                hal.free_dma(*b);
            }
            return Err(Error::msg(err));
        };
        bufs[i] = buf;
    }
    Ok(bufs)
}

/// Hardware Abstraction Layer for MMIO, Port I/O, DMA allocations, and cooperative async timing.
///
/// Drivers in `pixie-drivers` perform all hardware and timing interactions exclusively through
/// this trait so they can run with zero overhead on bare metal (`NativeHal`) and be tested
/// adversarially against stateful hardware simulators on the host.
pub trait Hal {
    fn mmio_read8(&self, addr: u64) -> u8;
    fn mmio_write8(&self, addr: u64, val: u8);
    fn mmio_read16(&self, addr: u64) -> u16;
    fn mmio_write16(&self, addr: u64, val: u16);
    fn mmio_read32(&self, addr: u64) -> u32;
    fn mmio_write32(&self, addr: u64, val: u32);
    fn mmio_read64(&self, addr: u64) -> u64;
    fn mmio_write64(&self, addr: u64, val: u64);

    fn io_read8(&self, port: u16) -> u8;
    fn io_write8(&self, port: u16, val: u8);
    fn io_read16(&self, port: u16) -> u16;
    fn io_write16(&self, port: u16, val: u16);
    fn io_read32(&self, port: u16) -> u32;
    fn io_write32(&self, port: u16, val: u32);

    /// Allocates `pages` contiguous 4096-byte pages for DMA, zero-initialized.
    /// Implementations should prefer physical addresses below 4 GiB for 32-bit DMA compatibility.
    fn alloc_dma(&self, pages: usize) -> Option<DmaBuffer>;

    /// Frees a previously allocated `DmaBuffer`.
    fn free_dma(&self, buf: DmaBuffer);

    /// Translates a physical address into a CPU-accessible virtual pointer.
    fn phys_to_virt(&self, phys: u64) -> *mut u8 {
        phys as *mut u8
    }

    /// Ensures an MMIO physical region of `[phys, phys + size)` is mapped uncacheable.
    fn map_mmio(&self, _phys: u64, _size: usize) {}

    /// Monotonic timestamp in microseconds.
    fn micros(&self) -> i64;

    /// Cooperative async yield to the executor.
    fn yield_now(&self) -> impl Future<Output = ()>;

    /// Cooperative async sleep for `duration`.
    fn sleep(&self, duration: Duration) -> impl Future<Output = ()>;
}

macro_rules! impl_hal_deref {
    ($($wrapper:ty),+ $(,)?) => {
        $(
            impl<T: Hal + ?Sized> Hal for $wrapper {
                fn mmio_read8(&self, addr: u64) -> u8 {
                    (**self).mmio_read8(addr)
                }
                fn mmio_write8(&self, addr: u64, val: u8) {
                    (**self).mmio_write8(addr, val)
                }
                fn mmio_read16(&self, addr: u64) -> u16 {
                    (**self).mmio_read16(addr)
                }
                fn mmio_write16(&self, addr: u64, val: u16) {
                    (**self).mmio_write16(addr, val)
                }
                fn mmio_read32(&self, addr: u64) -> u32 {
                    (**self).mmio_read32(addr)
                }
                fn mmio_write32(&self, addr: u64, val: u32) {
                    (**self).mmio_write32(addr, val)
                }
                fn mmio_read64(&self, addr: u64) -> u64 {
                    (**self).mmio_read64(addr)
                }
                fn mmio_write64(&self, addr: u64, val: u64) {
                    (**self).mmio_write64(addr, val)
                }
                fn io_read8(&self, port: u16) -> u8 {
                    (**self).io_read8(port)
                }
                fn io_write8(&self, port: u16, val: u8) {
                    (**self).io_write8(port, val)
                }
                fn io_read16(&self, port: u16) -> u16 {
                    (**self).io_read16(port)
                }
                fn io_write16(&self, port: u16, val: u16) {
                    (**self).io_write16(port, val)
                }
                fn io_read32(&self, port: u16) -> u32 {
                    (**self).io_read32(port)
                }
                fn io_write32(&self, port: u16, val: u32) {
                    (**self).io_write32(port, val)
                }
                fn alloc_dma(&self, pages: usize) -> Option<DmaBuffer> {
                    (**self).alloc_dma(pages)
                }
                fn free_dma(&self, buf: DmaBuffer) {
                    (**self).free_dma(buf)
                }
                fn phys_to_virt(&self, phys: u64) -> *mut u8 {
                    (**self).phys_to_virt(phys)
                }
                fn map_mmio(&self, phys: u64, size: usize) {
                    (**self).map_mmio(phys, size)
                }
                fn micros(&self) -> i64 {
                    (**self).micros()
                }
                fn yield_now(&self) -> impl Future<Output = ()> {
                    (**self).yield_now()
                }
                fn sleep(&self, duration: Duration) -> impl Future<Output = ()> {
                    (**self).sleep(duration)
                }
            }
        )+
    };
}

impl_hal_deref!(&T, Rc<T>, Arc<T>);
