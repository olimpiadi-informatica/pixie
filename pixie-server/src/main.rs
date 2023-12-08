pub mod dnsmasq;
pub mod http;
pub mod tcp;
pub mod udp;
pub mod ping;

use std::{
    fs::File,
    io::{BufRead, BufReader},
    net::{IpAddr, Ipv4Addr},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
};

use anyhow::{bail, Context, Result};
use clap::Parser;
use interfaces::Interface;
use ipnet::Ipv4Net;
use macaddr::MacAddr6;

use pixie_shared::{Config, Station, Unit};

use crate::dnsmasq::DnsmasqHandle;

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

#[derive(Parser, Debug)]
pub struct PixieOptions {
    /// Directory in which files will be stored.
    /// Must already contain files: tftpboot/uefi_app.efi, config.yaml
    #[clap(short, long, default_value = "./storage")]
    storage_dir: PathBuf,
}

#[actix_rt::main]
async fn main() -> Result<()> {
    env_logger::init();

    // Validate the configuration.
    let mut options = PixieOptions::parse();

    std::fs::create_dir_all(&options.storage_dir)
        .with_context(|| format!("create storage dir: {}", options.storage_dir.display()))?;

    options.storage_dir = std::fs::canonicalize(&options.storage_dir)
        .with_context(|| format!("storage dir is invalid: {}", options.storage_dir.display()))?;

    anyhow::ensure!(
        options.storage_dir.to_str().is_some(),
        "storage dir must be valid utf8"
    );

    for file_path in [["tftpboot", "uefi_app.efi"]] {
        let mut path = options.storage_dir.clone();
        for path_piece in file_path {
            path = path.join(path_piece);
        }
        anyhow::ensure!(path.is_file(), "{} not found", path.display());
    }

    let config: Config = {
        let config_path = options.storage_dir.join("config.yaml");
        let config = File::open(&config_path)
            .with_context(|| format!("open config file: {}", config_path.display()))?;
        serde_yaml::from_reader(&config)
            .with_context(|| format!("deserialize config from {}", config_path.display()))?
    };

    let mut dnsmasq_handle = DnsmasqHandle::from_config(&options.storage_dir, &config.dhcp)
        .context("Error start dnsmasq")?;

    let data = std::fs::read(options.storage_dir.join("registered.json"));
    let units: Vec<Unit> = data
        .ok()
        .map(|d| serde_json::from_slice(&d))
        .transpose()
        .context("invalid json at registered.json")?
        .unwrap_or_default();

    dnsmasq_handle.set_hosts(&units)?;

    let last = Mutex::new(Station {
        group: config.groups.iter().next().unwrap().0.clone(),
        image: config.images[0].clone(),
        ..Default::default()
    });

    let state = Arc::new(State {
        storage_dir: options.storage_dir,
        config,
        units: Mutex::new(units),
        dnsmasq_handle: Mutex::new(dnsmasq_handle),
        last,
    });

    tokio::select!(
        x = http::main(state.clone()) => x?,
        x = udp::main(&state) => x?,
        x = tcp::main(state.clone()) => x?,
        x = ping::main(&state) => x?,
    );

    Ok(())
}
