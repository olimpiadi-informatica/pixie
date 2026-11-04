use core::ptr;
use core::sync::atomic::{Ordering, fence};

use crate::error::{Error, Result};
use crate::hal::{DmaBuffer, Hal, PAGE_SIZE, alloc_dma_buffers};
use crate::net::is_valid_mac;

pub const NUM_RX_DESC: usize = 256;
pub const NUM_TX_DESC: usize = 128;
pub const BUFFER_SIZE: usize = 2048;

const CMD_EOP: u8 = 1 << 0;
const CMD_IFCS: u8 = 1 << 1;
const CMD_RS: u8 = 1 << 3;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RxDesc {
    pub buffer_addr: u64,
    pub length: u16,
    pub checksum: u16,
    pub status: u8,
    pub errors: u8,
    pub special: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TxDesc {
    pub buffer_addr: u64,
    pub length: u16,
    pub cso: u8,
    pub cmd: u8,
    pub status: u8,
    pub css: u8,
    pub special: u16,
}

pub(crate) fn read_ral_rah_mac<H: Hal>(
    hal: &H,
    mmio_base: u64,
    reg_ral: u64,
    reg_rah: u64,
) -> [u8; 6] {
    let ral = hal.mmio_read32(mmio_base + reg_ral);
    let rah = hal.mmio_read32(mmio_base + reg_rah);
    if ral != 0 && ral != 0xFFFF_FFFF {
        let candidate = [
            (ral & 0xFF) as u8,
            ((ral >> 8) & 0xFF) as u8,
            ((ral >> 16) & 0xFF) as u8,
            ((ral >> 24) & 0xFF) as u8,
            (rah & 0xFF) as u8,
            ((rah >> 8) & 0xFF) as u8,
        ];
        if is_valid_mac(&candidate) {
            return candidate;
        }
    }
    [0u8; 6]
}

pub(crate) fn write_mta_and_rar0<H: Hal>(
    hal: &H,
    mmio_base: u64,
    reg_mta: u64,
    reg_ral: u64,
    reg_rah: u64,
    mac: [u8; 6],
) {
    for i in 0..128 {
        hal.mmio_write32(mmio_base + reg_mta + (i * 4) as u64, 0);
    }
    let ral_val = (mac[0] as u32)
        | ((mac[1] as u32) << 8)
        | ((mac[2] as u32) << 16)
        | ((mac[3] as u32) << 24);
    let rah_val = (mac[4] as u32) | ((mac[5] as u32) << 8) | (1 << 31);
    hal.mmio_write32(mmio_base + reg_ral, ral_val);
    hal.mmio_write32(mmio_base + reg_rah, rah_val);
}

pub(crate) struct IntelRings {
    pub(crate) rx_ring: DmaBuffer,
    pub(crate) rx_bufs: DmaBuffer,
    pub(crate) rx_cur: usize,
    pub(crate) tx_ring: DmaBuffer,
    pub(crate) tx_bufs: DmaBuffer,
    pub(crate) tx_cur: usize,
    pub(crate) tx_clean: usize,
}

impl IntelRings {
    pub(crate) const RX_RING_BYTES: usize = NUM_RX_DESC * core::mem::size_of::<RxDesc>();
    pub(crate) const TX_RING_BYTES: usize = NUM_TX_DESC * core::mem::size_of::<TxDesc>();

    pub(crate) fn alloc<H: Hal>(hal: &H, err: &'static str) -> Result<Self> {
        let rx_ring_pages = Self::RX_RING_BYTES.div_ceil(PAGE_SIZE);
        let rx_buf_pages = (NUM_RX_DESC * BUFFER_SIZE).div_ceil(PAGE_SIZE);
        let tx_ring_pages = Self::TX_RING_BYTES.div_ceil(PAGE_SIZE);
        let tx_buf_pages = (NUM_TX_DESC * BUFFER_SIZE).div_ceil(PAGE_SIZE);

        let [rx_ring, rx_bufs, tx_ring, tx_bufs] = alloc_dma_buffers(
            hal,
            [rx_ring_pages, rx_buf_pages, tx_ring_pages, tx_buf_pages],
            err,
        )?;

        let rx_descs_ptr = rx_ring.virt as *mut RxDesc;
        for i in 0..NUM_RX_DESC {
            unsafe {
                ptr::write_volatile(
                    rx_descs_ptr.add(i),
                    RxDesc {
                        buffer_addr: rx_bufs.phys + (i * BUFFER_SIZE) as u64,
                        ..Default::default()
                    },
                );
            }
        }

        let tx_descs_ptr = tx_ring.virt as *mut TxDesc;
        for i in 0..NUM_TX_DESC {
            unsafe {
                ptr::write_volatile(
                    tx_descs_ptr.add(i),
                    TxDesc {
                        buffer_addr: tx_bufs.phys + (i * BUFFER_SIZE) as u64,
                        status: 1, // DD=1: slot initially free
                        ..Default::default()
                    },
                );
            }
        }
        fence(Ordering::SeqCst);

        Ok(Self {
            rx_ring,
            rx_bufs,
            rx_cur: 0,
            tx_ring,
            tx_bufs,
            tx_cur: 0,
            tx_clean: 0,
        })
    }

    pub(crate) fn sync_hw_tdh(&mut self, raw_tdh: u32) -> usize {
        let hw_tdh = if (raw_tdh as usize) < NUM_TX_DESC {
            raw_tdh as usize
        } else {
            0
        };
        self.tx_cur = hw_tdh;
        self.tx_clean = hw_tdh;
        hw_tdh
    }

    pub(crate) fn clean_tx(&mut self, raw_tdh: u32) {
        let tx_descs = self.tx_ring.virt as *mut TxDesc;
        while self.tx_clean != self.tx_cur {
            let status = unsafe { ptr::read_volatile(&(*tx_descs.add(self.tx_clean)).status) };
            if (status & 1) != 0 {
                self.tx_clean = (self.tx_clean + 1) % NUM_TX_DESC;
            } else {
                break;
            }
        }

        if self.tx_clean != self.tx_cur && (raw_tdh as usize) < NUM_TX_DESC {
            let tdh = raw_tdh as usize;
            let advanced = if self.tx_cur >= self.tx_clean {
                tdh > self.tx_clean && tdh <= self.tx_cur
            } else {
                tdh > self.tx_clean || tdh <= self.tx_cur
            };
            if advanced {
                while self.tx_clean != tdh {
                    unsafe {
                        ptr::write_volatile(&mut (*tx_descs.add(self.tx_clean)).status, 1);
                    }
                    self.tx_clean = (self.tx_clean + 1) % NUM_TX_DESC;
                }
            }
        }
    }

    pub(crate) fn can_transmit(&mut self, raw_tdh: u32) -> bool {
        self.clean_tx(raw_tdh);
        let tx_descs = self.tx_ring.virt as *const TxDesc;
        let cur_status = unsafe { ptr::read_volatile(&(*tx_descs.add(self.tx_cur)).status) };
        (self.tx_cur + 1) % NUM_TX_DESC != self.tx_clean && (cur_status & 1) != 0
    }

    pub(crate) fn transmit<F: FnOnce(u32)>(
        &mut self,
        packet: &[u8],
        raw_tdh: u32,
        size_err: &'static str,
        full_err: &'static str,
        write_tdt: F,
    ) -> Result<()> {
        if packet.len() > BUFFER_SIZE {
            return Err(Error::msg(size_err));
        }
        if !self.can_transmit(raw_tdh) {
            return Err(Error::msg(full_err));
        }

        let idx = self.tx_cur;
        let buf_phys = self.tx_bufs.phys + (idx * BUFFER_SIZE) as u64;
        let buf_ptr = unsafe { self.tx_bufs.virt.add(idx * BUFFER_SIZE) };
        let desc_ptr = unsafe { (self.tx_ring.virt as *mut TxDesc).add(idx) };

        unsafe {
            ptr::copy_nonoverlapping(packet.as_ptr(), buf_ptr, packet.len());
            ptr::write_volatile(
                desc_ptr,
                TxDesc {
                    buffer_addr: buf_phys,
                    length: packet.len() as u16,
                    cso: 0,
                    cmd: CMD_EOP | CMD_IFCS | CMD_RS,
                    status: 0,
                    css: 0,
                    special: 0,
                },
            );
        }
        fence(Ordering::SeqCst);

        self.tx_cur = (idx + 1) % NUM_TX_DESC;
        write_tdt(self.tx_cur as u32);
        Ok(())
    }

    pub(crate) fn has_packets(&self) -> bool {
        let rx_descs = self.rx_ring.virt as *const RxDesc;
        let status = unsafe { ptr::read_volatile(&(*rx_descs.add(self.rx_cur)).status) };
        (status & 1) != 0
    }

    pub(crate) fn receive<F: FnMut(u32)>(
        &mut self,
        buf: &mut [u8],
        mut write_rdt: F,
    ) -> Option<usize> {
        const RX_FATAL_ERRORS: u8 = 0x83;
        let rx_descs = self.rx_ring.virt as *mut RxDesc;

        for _ in 0..NUM_RX_DESC {
            let idx = self.rx_cur;
            let desc_ptr = unsafe { rx_descs.add(idx) };
            let desc = unsafe { ptr::read_volatile(desc_ptr) };
            if (desc.status & 1) == 0 {
                return None;
            }

            fence(Ordering::Acquire);
            let buf_phys = self.rx_bufs.phys + (idx * BUFFER_SIZE) as u64;

            if (desc.errors & RX_FATAL_ERRORS) != 0 || (desc.status & 2) == 0 {
                unsafe {
                    ptr::write_volatile(
                        desc_ptr,
                        RxDesc {
                            buffer_addr: buf_phys,
                            ..Default::default()
                        },
                    );
                }
                fence(Ordering::SeqCst);
                self.rx_cur = (idx + 1) % NUM_RX_DESC;
                write_rdt(idx as u32);
                continue;
            }

            let copy_len = (desc.length as usize).min(buf.len());
            let pkt_ptr = unsafe { self.rx_bufs.virt.add(idx * BUFFER_SIZE) };
            unsafe {
                ptr::copy_nonoverlapping(pkt_ptr, buf.as_mut_ptr(), copy_len);
                ptr::write_volatile(
                    desc_ptr,
                    RxDesc {
                        buffer_addr: buf_phys,
                        ..Default::default()
                    },
                );
            }
            fence(Ordering::SeqCst);

            self.rx_cur = (idx + 1) % NUM_RX_DESC;
            write_rdt(idx as u32);
            return Some(copy_len);
        }
        None
    }

    pub(crate) fn free<H: Hal>(&self, hal: &H) {
        hal.free_dma(self.rx_ring);
        hal.free_dma(self.rx_bufs);
        hal.free_dma(self.tx_ring);
        hal.free_dma(self.tx_bufs);
    }
}
