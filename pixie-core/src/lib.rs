pub mod dnsmasq;
pub mod http;
pub mod tcp;
pub mod udp;

use std::{
    io::{BufRead, BufReader},
    net::{IpAddr, Ipv4Addr},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::Mutex,
};

use anyhow::{bail, Result};
use interfaces::Interface;
use ipnet::Ipv4Net;
use macaddr::MacAddr6;

use pixie_shared::{Config, Station, Unit};

use dnsmasq::DnsmasqHandle;

pub struct State {
    pub storage_dir: PathBuf,
    pub config: Config,
    pub units: Mutex<Vec<Unit>>,
    pub dnsmasq_handle: Mutex<DnsmasqHandle>,
    // TODO: use an Option
    pub last: Mutex<Station>,
}

impl State {
    pub fn registered_file(&self) -> PathBuf {
        self.storage_dir.join("registered.json")
    }
}

pub fn find_mac(ip: Ipv4Addr) -> Result<MacAddr6> {
    struct Zombie {
        inner: Child,
    }

    impl Drop for Zombie {
        fn drop(&mut self) {
            self.inner.kill().unwrap();
            self.inner.wait().unwrap();
        }
    }

    if ip.is_loopback() {
        bail!("localhost not supported");
    }

    let s = ip.to_string();

    // Repeat twice, sending a ping if looking at ip neigh the first time fails.
    for _ in 0..2 {
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
                if let Ok(mac) = mac.parse() {
                    return Ok(mac);
                }
            }
        }

        let _ = Command::new("ping")
            .args([&s, "-c", "1", "-W", "0.1"])
            .stdout(Stdio::null())
            .spawn()?
            .wait();
    }

    bail!("Mac address not found");
}

pub fn find_network(peer_ip: Ipv4Addr) -> Result<Ipv4Net> {
    for interface in Interface::get_all()? {
        for address in &interface.addresses {
            let Some(IpAddr::V4(addr)) = address.addr.map(|x| x.ip()) else {
                continue;
            };
            let Some(IpAddr::V4(mask)) = address.mask.map(|x| x.ip()) else {
                continue;
            };
            let network = Ipv4Net::with_netmask(addr, mask).expect("invalid network mask");
            if network.contains(&peer_ip) {
                return Ok(network);
            }
        }
    }
    bail!("Could not find the network for {}", peer_ip);
}
