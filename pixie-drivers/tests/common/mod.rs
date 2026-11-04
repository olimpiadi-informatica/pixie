#![allow(dead_code)]

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use pixie_drivers::hal::{DmaBuffer, PAGE_SIZE};

pub fn block_on<F: Future>(mut fut: F) -> F::Output {
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

pub struct DmaPool {
    phys_base: u64,
    max_pages: usize,
    next_page: usize,
    freed_pages: usize,
    buf: Box<[u8]>,
}

impl DmaPool {
    pub fn new(phys_base: u64, max_pages: usize) -> Self {
        Self {
            phys_base,
            max_pages,
            next_page: 0,
            freed_pages: 0,
            buf: vec![0u8; max_pages * PAGE_SIZE].into_boxed_slice(),
        }
    }

    pub fn phys_base(&self) -> u64 {
        self.phys_base
    }

    pub fn allocated_pages(&self) -> usize {
        self.next_page
    }

    pub fn allocated_bytes(&self) -> usize {
        self.next_page * PAGE_SIZE
    }

    pub fn freed_pages(&self) -> usize {
        self.freed_pages
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn raw(&self) -> &[u8] {
        &self.buf
    }

    pub fn raw_mut(&mut self) -> &mut [u8] {
        &mut self.buf
    }

    pub fn alloc(&mut self, pages: usize) -> Option<DmaBuffer> {
        if self.next_page + pages > self.max_pages {
            return None;
        }
        let offset = self.next_page * PAGE_SIZE;
        let size = pages * PAGE_SIZE;
        self.next_page += pages;
        self.buf[offset..offset + size].fill(0);
        let virt = unsafe { self.buf.as_mut_ptr().add(offset) };
        let phys = self.phys_base + offset as u64;
        Some(DmaBuffer { virt, phys, size })
    }

    pub fn free(&mut self, buf: DmaBuffer) {
        self.freed_pages += buf.size / PAGE_SIZE;
    }

    pub fn phys_to_virt(&mut self, phys: u64) -> *mut u8 {
        assert!(
            phys >= self.phys_base,
            "DMA phys {:#x} below base {:#x}",
            phys,
            self.phys_base
        );
        let offset = (phys - self.phys_base) as usize;
        assert!(
            offset <= self.buf.len(),
            "DMA phys {:#x} out of pool bounds",
            phys
        );
        unsafe { self.buf.as_mut_ptr().add(offset) }
    }

    pub fn slice(&self, phys: u64, len: usize) -> &[u8] {
        assert!(
            phys >= self.phys_base,
            "DMA phys {:#x} below base {:#x}",
            phys,
            self.phys_base
        );
        let off = (phys - self.phys_base) as usize;
        assert!(
            off + len <= self.buf.len(),
            "DMA range {:#x}..{:#x} out of pool bounds",
            phys,
            phys + len as u64
        );
        &self.buf[off..off + len]
    }

    pub fn slice_mut(&mut self, phys: u64, len: usize) -> &mut [u8] {
        assert!(
            phys >= self.phys_base,
            "DMA phys {:#x} below base {:#x}",
            phys,
            self.phys_base
        );
        let off = (phys - self.phys_base) as usize;
        assert!(
            off + len <= self.buf.len(),
            "DMA range {:#x}..{:#x} out of pool bounds",
            phys,
            phys + len as u64
        );
        &mut self.buf[off..off + len]
    }
}
