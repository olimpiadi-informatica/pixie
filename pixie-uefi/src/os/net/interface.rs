use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;
use uefi::boot::ScopedProtocol;
use uefi::proto::network::snp::{ReceiveFlags, SimpleNetwork};
use uefi::Status;

use super::ETH_PACKET_SIZE;
use crate::os::send_wrapper::SendWrapper;

type Snp = SendWrapper<ScopedProtocol<SimpleNetwork>>;

pub struct SnpDevice {
    snp: Snp,
    tx_buf: [u8; ETH_PACKET_SIZE],
    // Received packets might contain Ethernet-related padding (up to 4 bytes).
    rx_buf: [u8; ETH_PACKET_SIZE + 4],
}

impl SnpDevice {
    pub fn new(snp: Snp) -> SnpDevice {
        // Shut down the SNP protocol if needed.
        let _ = snp.shutdown();
        let _ = snp.stop();
        // Initialize.
        snp.start().unwrap();
        snp.initialize(0, 0).unwrap();
        // Enable packet reception.
        snp.receive_filters(
            ReceiveFlags::UNICAST | ReceiveFlags::BROADCAST,
            ReceiveFlags::empty(),
            true,
            None,
        )
        .unwrap();

        SnpDevice {
            snp,
            tx_buf: [0; ETH_PACKET_SIZE],
            rx_buf: [0; ETH_PACKET_SIZE + 4],
        }
    }
}

impl Drop for SnpDevice {
    fn drop(&mut self) {
        self.snp.stop().unwrap()
    }
}

pub struct SnpRxToken<'a> {
    packet: &'a mut [u8],
}

pub struct SnpTxToken<'a> {
    snp: &'a Snp,
    buf: &'a mut [u8],
}

impl TxToken for SnpTxToken<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        assert!(len <= self.buf.len());
        let payload = &mut self.buf[..len];
        let ret = f(payload);
        let snp = self.snp;
        snp.transmit(0, payload, None, None, None)
            .expect("Failed to transmit frame");
        // Wait until sending is complete.
        while snp.get_recycled_transmit_buffer_status().unwrap().is_none() {}
        ret
    }
}

impl RxToken for SnpRxToken<'_> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(self.packet)
    }
}

impl Device for SnpDevice {
    type TxToken<'d> = SnpTxToken<'d>;
    type RxToken<'d> = SnpRxToken<'d>;

    fn receive(&mut self, _: Instant) -> Option<(SnpRxToken<'_>, SnpTxToken<'_>)> {
        let rec = self.snp.receive(&mut self.rx_buf, None, None, None, None);
        if rec == Err(Status::NOT_READY.into()) {
            return None;
        }
        Some((
            SnpRxToken {
                packet: &mut self.rx_buf[..rec.unwrap()],
            },
            SnpTxToken {
                snp: &self.snp,
                buf: &mut self.tx_buf,
            },
        ))
    }

    fn transmit(&mut self, _: Instant) -> Option<SnpTxToken<'_>> {
        Some(SnpTxToken {
            snp: &self.snp,
            buf: &mut self.tx_buf,
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        let mode = self.snp.mode();
        assert!(mode.media_header_size == 14);
        caps.max_transmission_unit =
            ETH_PACKET_SIZE.min((mode.max_packet_size + mode.media_header_size) as usize);
        caps.max_burst_size = Some(1);
        caps
    }
}
