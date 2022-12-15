pub mod dnsmasq;
pub mod http;
pub mod udp;

use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader},
    net::{IpAddr, Ipv4Addr},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Mutex, RwLock},
};

use anyhow::{bail, Result};
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

pub fn find_mac(ip: IpAddr) -> Result<MacAddr6> {
    struct Zombie {
        inner: Child,
    }

    impl Drop for Zombie {
        fn drop(&mut self) {
            self.inner.kill().unwrap();
            self.inner.wait().unwrap();
        }
    }

    if ip == IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)) {
        bail!("localhost not supported");
    }

    let s = ip.to_string();

    let mut child = Zombie {
        inner: Command::new("ip")
            .arg("neigh")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?,
    };
    let stdout = child.inner.stdout.take().unwrap();
    let lines = BufReader::new(stdout).lines();

    for line in lines {
        let line = line?;
        let mut parts = line.split(' ');

        if parts.next() == Some(&s) {
            let mac = parts.nth(3).unwrap();
            return Ok(mac.parse().unwrap());
        }
    }

    bail!("Mac address not found");
}
