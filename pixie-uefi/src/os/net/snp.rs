use alloc::boxed::Box;
use alloc::vec::Vec;
use uefi::Status;
use uefi::boot::ScopedProtocol;
use uefi::proto::network::snp::{ReceiveFlags, SimpleNetwork};

use super::ETH_PACKET_SIZE;
use crate::os::send_wrapper::SendWrapper;
use crate::os::{input, ui};
use crate::power_control;

type Snp = SendWrapper<ScopedProtocol<SimpleNetwork>>;

pub const RX_POOL_SIZE: usize = 1024;
const TX_POOL_SIZE: usize = 32;

pub struct RxPool {
    bufs: Box<[[u8; ETH_PACKET_SIZE + 4]; RX_POOL_SIZE]>,
    free: Vec<u16>,
}

impl RxPool {
    pub fn new() -> Self {
        let bufs: Box<[[u8; ETH_PACKET_SIZE + 4]; RX_POOL_SIZE]> =
            vec![[0u8; ETH_PACKET_SIZE + 4]; RX_POOL_SIZE]
                .into_boxed_slice()
                .try_into()
                .unwrap();
        let free = (0..RX_POOL_SIZE as u16).rev().collect();
        Self { bufs, free }
    }

    #[inline]
    pub fn alloc(&mut self) -> Option<u16> {
        self.free.pop()
    }

    #[inline]
    pub fn free(&mut self, idx: u16) {
        self.free.push(idx);
    }

    #[inline]
    pub fn buf(&self, idx: u16) -> &[u8] {
        &self.bufs[idx as usize]
    }

    #[inline]
    pub fn buf_mut(&mut self, idx: u16) -> &mut [u8] {
        &mut self.bufs[idx as usize]
    }
}

pub struct SnpNic {
    snp: Snp,
    mac: [u8; 6],
    tx_bufs: Box<[[u8; ETH_PACKET_SIZE]; TX_POOL_SIZE]>,
    in_flight: [bool; TX_POOL_SIZE],
    free: Vec<usize>,
}

impl SnpNic {
    pub fn new(snp: Snp) -> Self {
        let _ = snp.shutdown();
        let _ = snp.stop();
        snp.start().unwrap();
        snp.initialize(0, 0).unwrap();
        snp.receive_filters(
            ReceiveFlags::UNICAST | ReceiveFlags::BROADCAST,
            ReceiveFlags::empty(),
            true,
            None,
        )
        .unwrap();

        let mode = snp.mode();
        assert!(mode.media_header_size == 14);
        let mac: [u8; 6] = mode.current_address.0[..6].try_into().unwrap();

        let tx_bufs: Box<[[u8; ETH_PACKET_SIZE]; TX_POOL_SIZE]> =
            vec![[0u8; ETH_PACKET_SIZE]; TX_POOL_SIZE]
                .into_boxed_slice()
                .try_into()
                .unwrap();
        let free = (0..TX_POOL_SIZE).rev().collect();

        Self {
            snp,
            mac,
            tx_bufs,
            in_flight: [false; TX_POOL_SIZE],
            free,
        }
    }

    #[inline]
    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }

    fn check_media_present(&self) {
        if self.snp.mode().media_present.0 == 0 {
            let err = uefi::boot::set_watchdog_timer(0, 0x10000, None);
            if let Err(err) = err
                && err.status() != Status::UNSUPPORTED
            {
                log::error!("Error disabling watchdog: {err:?}");
            }
            ui::red_screen();
            input::wait_for_key().expect("Failed to wait for input");
            power_control::reset();
        }
    }

    pub fn drain_recycled(&mut self) {
        let base = self.tx_bufs.as_ptr() as usize;
        while let Ok(Some(ptr)) = self.snp.get_recycled_transmit_buffer_status() {
            let addr = ptr.as_ptr() as usize;
            if addr >= base {
                let offset = addr - base;
                let idx = offset / ETH_PACKET_SIZE;
                if offset.is_multiple_of(ETH_PACKET_SIZE)
                    && idx < TX_POOL_SIZE
                    && self.in_flight[idx]
                {
                    self.in_flight[idx] = false;
                    self.free.push(idx);
                }
            }
        }
    }

    pub fn transmit(&mut self, build: impl FnOnce(&mut [u8]) -> usize) {
        self.drain_recycled();
        while self.free.is_empty() {
            self.check_media_present();
            self.drain_recycled();
        }
        let idx = self.free.pop().unwrap();
        let len = build(&mut self.tx_bufs[idx]);
        assert!(len <= ETH_PACKET_SIZE);

        loop {
            match self
                .snp
                .transmit(0, &self.tx_bufs[idx][..len], None, None, None)
            {
                Ok(()) => {
                    self.in_flight[idx] = true;
                    return;
                }
                Err(e) if e.status() == Status::NOT_READY => {
                    self.check_media_present();
                    self.drain_recycled();
                }
                Err(e) => panic!("Failed to transmit frame: {e:?}"),
            }
        }
    }

    #[inline]
    pub fn try_recv(&mut self, buf: &mut [u8]) -> Option<usize> {
        match self.snp.receive(buf, None, None, None, None) {
            Ok(len) => Some(len),
            Err(e) if e.status() == Status::NOT_READY => None,
            Err(e) => panic!("Failed to receive frame: {e:?}"),
        }
    }
}

impl Drop for SnpNic {
    fn drop(&mut self) {
        let _ = self.snp.stop();
    }
}
