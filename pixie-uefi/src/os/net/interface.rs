use uefi::Status;
use uefi::boot::ScopedProtocol;
use uefi::proto::network::snp::{ReceiveFlags, SimpleNetwork};

use super::ETH_PACKET_SIZE;
use crate::os::send_wrapper::SendWrapper;
use crate::os::{input, ui};
use crate::power_control;

type Snp = SendWrapper<ScopedProtocol<SimpleNetwork>>;

pub struct SnpDevice {
    snp: Snp,
    tx_buf: [u8; ETH_PACKET_SIZE],
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

        let mode = snp.mode();
        assert!(mode.media_header_size == 14);

        SnpDevice {
            snp,
            tx_buf: [0; ETH_PACKET_SIZE],
        }
    }

    pub fn receive<'a>(&self, rx_buf: &'a mut [u8; ETH_PACKET_SIZE + 4]) -> Option<&'a [u8]> {
        let rec = self.snp.receive(rx_buf, None, None, None, None);
        if rec == Err(Status::NOT_READY.into()) {
            return None;
        }
        Some(&rx_buf[..rec.unwrap()])
    }

    pub fn transmit_with<F: FnOnce(&mut [u8]) -> usize>(&mut self, f: F) {
        let len = f(&mut self.tx_buf);
        assert!(len <= self.tx_buf.len());
        let payload = &self.tx_buf[..len];
        self.snp
            .transmit(0, payload, None, None, None)
            .expect("Failed to transmit frame");
        // Wait until sending is complete.
        while self
            .snp
            .get_recycled_transmit_buffer_status()
            .unwrap()
            .is_none()
        {
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
    }
}

impl Drop for SnpDevice {
    fn drop(&mut self) {
        self.snp.stop().unwrap()
    }
}
