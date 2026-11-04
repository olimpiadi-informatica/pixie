use crate::hal::NativeHal;
use crate::net::ETH_PACKET_SIZE;
use pixie_drivers::net::e1000::E1000Device;
use pixie_drivers::net::igc::IgcDevice;
use pixie_drivers::net::rtl::RtlDevice;
use pixie_drivers::net::usb_net::UsbEthernetDevice;

pub enum KernelNic {
    E1000(E1000Device<NativeHal>),
    Igc(IgcDevice<NativeHal>),
    Rtl(RtlDevice<NativeHal>),
    Usb(UsbEthernetDevice<NativeHal>),
}

impl KernelNic {
    pub fn mac_address(&self) -> [u8; 6] {
        match self {
            Self::E1000(d) => d.mac_address(),
            Self::Igc(d) => d.mac_address(),
            Self::Rtl(d) => d.mac_address(),
            Self::Usb(d) => d.mac_address(),
        }
    }

    pub fn can_transmit(&mut self) -> bool {
        match self {
            Self::E1000(d) => d.can_transmit(),
            Self::Igc(d) => d.can_transmit(),
            Self::Rtl(d) => d.can_transmit(),
            Self::Usb(d) => d.can_transmit(),
        }
    }

    pub fn transmit(&mut self, packet: &[u8]) -> pixie_drivers::error::Result<()> {
        match self {
            Self::E1000(d) => d.transmit(packet),
            Self::Igc(d) => d.transmit(packet),
            Self::Rtl(d) => d.transmit(packet),
            Self::Usb(d) => d.transmit(packet),
        }
    }

    pub fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        match self {
            Self::E1000(d) => d.receive(buf),
            Self::Igc(d) => d.receive(buf),
            Self::Rtl(d) => d.receive(buf),
            Self::Usb(d) => d.receive(buf),
        }
    }

    pub fn has_packets(&mut self) -> bool {
        match self {
            Self::E1000(d) => d.has_packets(),
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
    pub fn receive<'a>(&mut self, rx_buf: &'a mut [u8; ETH_PACKET_SIZE + 4]) -> Option<&'a [u8]> {
        let len = self.nic.receive(rx_buf)?;
        if len == 0 {
            return None;
        }
        Some(&rx_buf[..len.min(ETH_PACKET_SIZE)])
    }

    pub fn transmit_with<F: FnOnce(&mut [u8]) -> usize>(&mut self, f: F) {
        let len = f(&mut self.tx_buf);
        let clamped = len.min(self.tx_buf.len());
        let _ = self.nic.transmit(&self.tx_buf[..clamped]);
    }
}
