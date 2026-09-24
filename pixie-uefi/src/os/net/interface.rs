use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;

use super::ETH_PACKET_SIZE;
use super::e1000e::E1000Device;
use super::rtl8169::Rtl8169Device;

#[allow(clippy::large_enum_variant)]
pub enum KernelNic {
    E1000(E1000Device),
    Rtl8169(Rtl8169Device),
}

impl KernelNic {
    pub fn mac_address(&self) -> [u8; 6] {
        match self {
            KernelNic::E1000(d) => d.mac_address(),
            KernelNic::Rtl8169(d) => d.mac_address(),
        }
    }

    pub fn is_link_up(&self) -> bool {
        match self {
            KernelNic::E1000(d) => d.is_link_up(),
            KernelNic::Rtl8169(d) => d.is_link_up(),
        }
    }

    pub fn transmit(&mut self, packet: &[u8]) {
        match self {
            KernelNic::E1000(d) => d.transmit(packet),
            KernelNic::Rtl8169(d) => d.transmit(packet),
        }
    }

    pub fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        match self {
            KernelNic::E1000(d) => d.receive(buf),
            KernelNic::Rtl8169(d) => d.receive(buf),
        }
    }

    pub fn has_packets(&self) -> bool {
        match self {
            KernelNic::E1000(d) => d.has_packets(),
            KernelNic::Rtl8169(d) => d.has_packets(),
        }
    }

    pub fn can_transmit(&mut self) -> bool {
        match self {
            KernelNic::E1000(d) => d.can_transmit(),
            KernelNic::Rtl8169(d) => d.can_transmit(),
        }
    }
}

pub struct KernelNicDevice {
    pub nic: KernelNic,
    tx_buf: [u8; ETH_PACKET_SIZE],
    rx_buf: [u8; ETH_PACKET_SIZE + 4],
}

impl KernelNicDevice {
    pub fn new(nic: KernelNic) -> Self {
        Self {
            nic,
            tx_buf: [0; ETH_PACKET_SIZE],
            rx_buf: [0; ETH_PACKET_SIZE + 4],
        }
    }
}

pub struct KernelRxToken<'a> {
    packet: &'a mut [u8],
}

impl<'a> RxToken for KernelRxToken<'a> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(self.packet)
    }
}

pub struct KernelTxToken<'a> {
    nic: &'a mut KernelNic,
    buf: &'a mut [u8],
}

impl<'a> TxToken for KernelTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        assert!(len <= self.buf.len());
        let payload = &mut self.buf[..len];
        let ret = f(payload);
        self.nic.transmit(payload);
        ret
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
        let rec = self.nic.receive(&mut self.rx_buf);
        rec.map(|len| {
            (
                KernelRxToken {
                    packet: &mut self.rx_buf[..len],
                },
                KernelTxToken {
                    nic: &mut self.nic,
                    buf: &mut self.tx_buf,
                },
            )
        })
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        if !self.nic.can_transmit() {
            return None;
        }
        Some(KernelTxToken {
            nic: &mut self.nic,
            buf: &mut self.tx_buf,
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
