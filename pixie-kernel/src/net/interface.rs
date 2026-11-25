use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::hal::NativeHal;
use crate::net::ETH_PACKET_SIZE;
use pixie_drivers::net::e1000::E1000Device;
use pixie_drivers::net::igb::IgbDevice;
use pixie_drivers::net::igc::IgcDevice;
use pixie_drivers::net::rtl::RtlDevice;
use pixie_drivers::net::usb_net::UsbEthernetDevice;

pub const RX_POOL_SIZE: usize = 1024;

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

impl Default for RxPool {
    fn default() -> Self {
        Self::new()
    }
}

pub enum KernelNic {
    E1000(E1000Device<NativeHal>),
    Igb(IgbDevice<NativeHal>),
    Igc(IgcDevice<NativeHal>),
    Rtl(RtlDevice<NativeHal>),
    Usb(UsbEthernetDevice<NativeHal>),
}

impl KernelNic {
    pub fn mac_address(&self) -> [u8; 6] {
        match self {
            Self::E1000(d) => d.mac_address(),
            Self::Igb(d) => d.mac_address(),
            Self::Igc(d) => d.mac_address(),
            Self::Rtl(d) => d.mac_address(),
            Self::Usb(d) => d.mac_address(),
        }
    }

    pub fn can_transmit(&mut self) -> bool {
        match self {
            Self::E1000(d) => d.can_transmit(),
            Self::Igb(d) => d.can_transmit(),
            Self::Igc(d) => d.can_transmit(),
            Self::Rtl(d) => d.can_transmit(),
            Self::Usb(d) => d.can_transmit(),
        }
    }

    pub fn transmit(&mut self, packet: &[u8]) -> pixie_drivers::error::Result<()> {
        match self {
            Self::E1000(d) => d.transmit(packet),
            Self::Igb(d) => d.transmit(packet),
            Self::Igc(d) => d.transmit(packet),
            Self::Rtl(d) => d.transmit(packet),
            Self::Usb(d) => d.transmit(packet),
        }
    }

    pub fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        match self {
            Self::E1000(d) => d.receive(buf),
            Self::Igb(d) => d.receive(buf),
            Self::Igc(d) => d.receive(buf),
            Self::Rtl(d) => d.receive(buf),
            Self::Usb(d) => d.receive(buf),
        }
    }

    pub fn has_packets(&mut self) -> bool {
        match self {
            Self::E1000(d) => d.has_packets(),
            Self::Igb(d) => d.has_packets(),
            Self::Igc(d) => d.has_packets(),
            Self::Rtl(d) => d.has_packets(),
            Self::Usb(d) => d.has_packets(),
        }
    }
}

pub struct KernelNicDevice {
    pub nic: KernelNic,
    pub tx_buf: [u8; ETH_PACKET_SIZE],
}

impl KernelNicDevice {
    pub fn mac(&self) -> [u8; 6] {
        self.nic.mac_address()
    }

    pub fn try_recv(&mut self, rx_buf: &mut [u8]) -> Option<usize> {
        let len = self.nic.receive(rx_buf)?;
        if len == 0 {
            return None;
        }
        Some(len.min(ETH_PACKET_SIZE))
    }

    pub fn transmit(&mut self, build: impl FnOnce(&mut [u8]) -> usize) {
        let len = build(&mut self.tx_buf);
        if len == 0 {
            return;
        }
        let clamped = len.min(self.tx_buf.len());
        let start = crate::timer::Timer::micros();
        while !self.nic.can_transmit() {
            if crate::timer::Timer::micros().saturating_sub(start) > 10_000 {
                return;
            }
            crate::arch::io::pause();
        }
        let _ = self.nic.transmit(&self.tx_buf[..clamped]);
    }
}
