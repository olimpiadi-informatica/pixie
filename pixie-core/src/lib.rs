pub mod dnsmasq;
pub mod http;
pub mod udp;

use std::{
    io::{BufRead, BufReader},
    net::{IpAddr, Ipv4Addr},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::Mutex,
};

use anyhow::{anyhow, bail, Result};
use interfaces::Interface;
use macaddr::MacAddr6;

use pixie_shared::{ActionKind, PersistentServerState, Station};

use dnsmasq::DnsmasqHandle;

pub struct State {
    pub storage_dir: PathBuf,
    pub persistent: Mutex<PersistentServerState>,
    pub dnsmasq_handle: Mutex<DnsmasqHandle>,
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

    if ip == Ipv4Addr::new(127, 0, 0, 1) {
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

pub fn find_interface_ip(peer_ip: Ipv4Addr) -> Result<Ipv4Addr> {
    for interface in Interface::get_all()? {
        for address in &interface.addresses {
            let Some(IpAddr::V4(addr)) = address.addr.map(|x| x.ip()) else {
                continue;
            };
            let Some(IpAddr::V4(mask)) = address.mask.map(|x| x.ip()) else {
                continue;
            };
            if (u32::from_ne_bytes(addr.octets()) ^ u32::from_ne_bytes(peer_ip.octets()))
                & u32::from_ne_bytes(mask.octets())
                == 0
            {
                return Ok(addr);
            }
        }
    }
    Err(anyhow!("Could not find the corresponding ip"))?
}
