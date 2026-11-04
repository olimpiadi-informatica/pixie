use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;

use crate::hal::NativeHal;
use crate::net::ETH_PACKET_SIZE;
use pixie_drivers::net::e1000::E1000Device;
use pixie_drivers::net::rtl::RtlDevice;

#[allow(clippy::large_enum_variant)]
pub enum KernelNic {
    E1000(E1000Device<NativeHal>),
    Rtl(RtlDevice<NativeHal>),
}

impl KernelNic {
    pub fn mac_address(&self) -> [u8; 6] {
        match self {
            Self::E1000(d) => d.mac_address(),
            Self::Rtl(d) => d.mac_address(),
        }
    }

    pub fn can_transmit(&mut self) -> bool {
        match self {
            Self::E1000(d) => d.can_transmit(),
            Self::Rtl(d) => d.can_transmit(),
        }
    }

    pub fn transmit(&mut self, packet: &[u8]) -> pixie_drivers::error::Result<()> {
        match self {
            Self::E1000(d) => d.transmit(packet),
            Self::Rtl(d) => d.transmit(packet),
        }
    }

    pub fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        match self {
            Self::E1000(d) => d.receive(buf),
            Self::Rtl(d) => d.receive(buf),
        }
    }

    pub fn has_packets(&mut self) -> bool {
        match self {
            Self::E1000(d) => d.has_packets(),
            Self::Rtl(d) => d.has_packets(),
        }
    }
}

pub struct KernelNicDevice {
    pub nic: KernelNic,
    pub tx_buf: [u8; ETH_PACKET_SIZE],
    pub rx_buf: [u8; ETH_PACKET_SIZE + 4],
}

pub struct KernelRxToken<'a> {
    pub packet: &'a mut [u8],
}

impl RxToken for KernelRxToken<'_> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(self.packet)
    }
}

pub struct KernelTxToken<'a> {
    pub nic: &'a mut KernelNic,
    pub buf: &'a mut [u8],
}

impl TxToken for KernelTxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let clamped = len.min(self.buf.len());
        let result = f(&mut self.buf[..clamped]);
        let _ = self.nic.transmit(&self.buf[..clamped]);
        result
    }
}

impl Device for KernelNicDevice {
    type RxToken<'d>
        = KernelRxToken<'d>
    where
        Self: 'd;
    type TxToken<'d>
        = KernelTxToken<'d>
    where
        Self: 'd;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if !self.nic.can_transmit() {
            return None;
        }
        match self.nic.receive(&mut self.rx_buf) {
            Some(len) if len > 0 => {
                let clamped = len.min(ETH_PACKET_SIZE);
                Some((
                    KernelRxToken {
                        packet: &mut self.rx_buf[..clamped],
                    },
                    KernelTxToken {
                        nic: &mut self.nic,
                        buf: &mut self.tx_buf[..],
                    },
                ))
            }
            _ => None,
        }
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        if !self.nic.can_transmit() {
            return None;
        }
        Some(KernelTxToken {
            nic: &mut self.nic,
            buf: &mut self.tx_buf[..],
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = ETH_PACKET_SIZE;
        caps.max_burst_size = None;
        caps
    }
}
