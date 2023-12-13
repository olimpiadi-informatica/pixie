pub mod dnsmasq;
pub mod http;
pub mod ping;
pub mod state;
pub mod tcp;
pub mod udp;

use std::{
    fs,
    io::{BufRead, BufReader},
    net::{IpAddr, Ipv4Addr},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::Arc,
};

use anyhow::{bail, ensure, Context, Result};
use clap::Parser;
use interfaces::Interface;
use ipnet::Ipv4Net;
use macaddr::MacAddr6;

use crate::state::State;

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
    /// Must already contain files: tftpboot/pixie-uefi.efi, config.yaml
    #[clap(short, long, default_value = "./storage")]
    storage_dir: PathBuf,
}

#[actix_rt::main]
async fn main() -> Result<()> {
    env_logger::init();

    let mut options = PixieOptions::parse();

    options.storage_dir = fs::canonicalize(&options.storage_dir)
        .with_context(|| format!("storage dir is invalid: {}", options.storage_dir.display()))?;

    ensure!(
        options.storage_dir.to_str().is_some(),
        "storage dir must be valid utf8"
    );

    for file_path in [["tftpboot", "pixie-uefi.efi"]] {
        let mut path = options.storage_dir.clone();
        for path_piece in file_path {
            path = path.join(path_piece);
        }
        ensure!(path.is_file(), "{} not found", path.display());
    }

    let state = Arc::new(State::load(options.storage_dir)?);

    tokio::select! {
        x = dnsmasq::main(state.clone()) => x?,
        x = http::main(state.clone()) => x?,
        x = udp::main(&state) => x?,
        x = tcp::main(state.clone()) => x?,
        x = ping::main(&state) => x?,
    };

    Ok(())
}
