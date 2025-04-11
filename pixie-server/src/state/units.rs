use crate::state::State;
use macaddr::MacAddr6;
use pixie_shared::Station;

impl State {
    pub fn set_unit_ping(&self, peer_mac: MacAddr6, time: u64, message: Vec<u8>) {
        self.units.send_if_modified(|units| {
            let Some(unit) = units.iter_mut().find(|unit| unit.mac == peer_mac) else {
                log::warn!("Got ping from unknown unit");
                return false;
            };

            unit.last_ping_timestamp = time;
            unit.last_ping_msg = message;

            true
        });
    }

    pub fn get_last(&self) -> Station {
        self.last.lock().expect("last mutex is poisoned").clone()
    }

    pub fn set_last(&self, station: Station) {
        *self.last.lock().expect("last mutex is poisoned") = station;
    }
}
