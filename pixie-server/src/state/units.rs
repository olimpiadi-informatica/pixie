use crate::state::State;
use macaddr::MacAddr6;
use pixie_shared::{ActionKind, Station, Unit};
use std::net::Ipv4Addr;
use tokio::sync::watch;

pub enum UnitSelector {
    MacAddr(MacAddr6),
    IpAddr(Ipv4Addr),
    All,
    Group(u8),
    Image(String),
}

impl UnitSelector {
    pub fn parse(state: &State, selector: String) -> Option<UnitSelector> {
        if let Ok(mac) = selector.parse::<MacAddr6>() {
            Some(UnitSelector::MacAddr(mac))
        } else if let Ok(ip) = selector.parse::<Ipv4Addr>() {
            Some(UnitSelector::IpAddr(ip))
        } else if selector == "all" {
            Some(UnitSelector::All)
        } else if let Some(&group) = state.config.groups.get_by_first(&selector) {
            Some(UnitSelector::Group(group))
        } else if state.config.images.contains(&selector) {
            Some(UnitSelector::Image(selector))
        } else {
            None
        }
    }

    pub fn select(&self, unit: &Unit) -> bool {
        match self {
            UnitSelector::MacAddr(mac) => unit.mac == *mac,
            UnitSelector::IpAddr(ip) => unit.static_ip() == *ip,
            UnitSelector::All => true,
            UnitSelector::Group(group) => unit.group == *group,
            UnitSelector::Image(image) => unit.image == *image,
        }
    }
}

impl State {
    pub fn get_units(&self, selector: UnitSelector) -> Vec<Unit> {
        self.units
            .borrow()
            .iter()
            .filter(|unit| selector.select(unit))
            .cloned()
            .collect()
    }

    pub fn subscribe_units(&self) -> watch::Receiver<Vec<Unit>> {
        self.units.subscribe()
    }

    fn set_unit_inner(&self, selector: UnitSelector, f: impl Fn(&mut Unit)) -> usize {
        let mut updated = 0;
        self.units.send_if_modified(|units| {
            for unit in units {
                if selector.select(unit) {
                    f(unit);
                    updated += 1;
                }
            }
            updated > 0
        });
        updated
    }

    pub fn set_unit_ping(&self, selector: UnitSelector, time: u64, comment: &[u8]) -> usize {
        self.set_unit_inner(selector, |unit| {
            unit.last_ping_timestamp = time;
            unit.last_ping_comment = comment.to_owned();
        })
    }

    pub fn set_unit_next_action(&self, selector: UnitSelector, action: ActionKind) -> usize {
        self.set_unit_inner(selector, |unit| {
            unit.next_action = action;
        })
    }

    pub fn set_unit_image(&self, selector: UnitSelector, image: &str) -> usize {
        // TODO: check that image is valid
        self.set_unit_inner(selector, |unit| {
            unit.image = image.to_owned();
        })
    }

    pub fn set_unit_progress(
        &self,
        selector: UnitSelector,
        progress: Option<(usize, usize)>,
    ) -> usize {
        self.set_unit_inner(selector, |unit| {
            unit.curr_progress = progress;
        })
    }

    pub fn forget_unit(&self, selector: UnitSelector) -> usize {
        let mut updated = 0;
        self.units.send_if_modified(|units| {
            let len_before = units.len();
            units.retain(|unit| !selector.select(unit));
            updated = len_before - units.len();
            updated > 0
        });
        updated
    }

    pub fn get_last(&self) -> Station {
        self.last.lock().expect("last mutex is poisoned").clone()
    }

    pub fn set_last(&self, station: Station) {
        *self.last.lock().expect("last mutex is poisoned") = station;
    }
}
