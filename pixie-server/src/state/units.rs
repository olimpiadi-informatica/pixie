use crate::state::State;
use anyhow::{bail, ensure, Result};
use macaddr::MacAddr6;
use pixie_shared::{Action, ActionKind, Station, Unit};
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

    pub fn register_unit(&self, mac: MacAddr6, station: Station) -> Result<()> {
        if !self.config.images.contains(&station.image) {
            bail!("Unknown image: {}", station.image);
        }
        let Some(&group) = self.config.groups.get_by_first(&station.group) else {
            bail!("Unknown group: {}", station.group);
        };

        let mut res = Ok(());
        self.units.send_modify(|units| {
            res = (|| {
                ensure!(
                    !units.iter().any(|unit| unit.group == group
                        && unit.row == station.row
                        && unit.col == station.col),
                    "Duplicated IP address"
                );
                if let Some(unit) = units.iter_mut().find(|unit| unit.mac == mac) {
                    unit.group = group;
                    unit.row = station.row;
                    unit.col = station.col;
                    unit.image = station.image;
                } else {
                    let unit = Unit {
                        mac,
                        group,
                        row: station.row,
                        col: station.col,
                        curr_action: None,
                        curr_progress: None,
                        next_action: ActionKind::Wait,
                        image: station.image,
                        last_ping_timestamp: 0,
                        last_ping_comment: Vec::new(),
                    };
                    units.push(unit);
                }
                Ok(())
            })();
        });
        res
    }

    pub fn unit_complete_action(&self, selector: UnitSelector) -> usize {
        self.set_unit_inner(selector, |unit| {
            unit.curr_action = None;
            unit.curr_progress = None;
        })
    }

    pub fn get_unit_action(&self, peer_mac: MacAddr6) -> Action {
        let mut action = Action::Wait;
        self.units.send_if_modified(|units| {
            let unit = units.iter_mut().find(|unit| unit.mac == peer_mac);

            let modified;

            if let Some(unit) = unit {
                let action_kind = if let Some(action) = unit.curr_action {
                    modified = false;
                    action
                } else {
                    match unit.next_action {
                        ActionKind::Push | ActionKind::Pull | ActionKind::Register => {
                            unit.curr_action = Some(unit.next_action);
                            unit.next_action = ActionKind::Wait;
                            modified = true;
                        }
                        ActionKind::Reboot | ActionKind::Wait => {
                            modified = false;
                        }
                    }
                    unit.next_action
                };

                action = match action_kind {
                    ActionKind::Reboot => Action::Reboot,
                    ActionKind::Register => Action::Register,
                    ActionKind::Push => Action::Push {
                        image: unit.image.clone(),
                    },
                    ActionKind::Pull => Action::Pull {
                        image: unit.image.clone(),
                    },
                    ActionKind::Wait => Action::Wait,
                };
            } else {
                action = Action::Register;
                modified = false;
            }

            modified
        });
        action
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

    pub fn set_unit_image(&self, selector: UnitSelector, image: String) -> Result<usize> {
        ensure!(
            self.config.images.contains(&image),
            "Unknown image: {image}"
        );
        Ok(self.set_unit_inner(selector, |unit| {
            unit.image = image.clone();
        }))
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
