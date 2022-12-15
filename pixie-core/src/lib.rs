pub mod dnsmasq;
pub mod http;
pub mod udp;

use std::{
    collections::BTreeMap,
    net::Ipv4Addr,
    path::PathBuf,
    sync::{Mutex, RwLock},
};

use macaddr::MacAddr6;
use serde::{Deserialize, Serialize};

use pixie_shared::{Station, StationKind};

use dnsmasq::DnsmasqHandle;

#[derive(Debug, Serialize, Deserialize)]
pub struct Unit {
    pub mac: MacAddr6,
    pub kind: StationKind,
    pub group: u8,
    pub row: u8,
    pub col: u8,
    pub action: String,
}

impl Unit {
    pub fn ip(&self) -> Ipv4Addr {
        Ipv4Addr::new(10, self.group, self.row, self.col)
    }
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub dnsmasq: dnsmasq::Config,
    pub http: http::Config,
    pub udp: udp::Config,
    pub boot: http::BootConfig,
    pub groups: BTreeMap<String, u8>,
}

pub struct State {
    pub storage_dir: PathBuf,
    pub config: Config,
    pub units: RwLock<Vec<Unit>>,
    pub dnsmasq_handle: Mutex<DnsmasqHandle>,
    pub hint: Mutex<Station>,
}

impl State {
    pub fn registered_file(&self) -> PathBuf {
        self.storage_dir.join("registered.json")
    }
}
