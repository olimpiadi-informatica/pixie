mod dnsmasq;
mod http;
mod ping;
mod state;
mod tcp;
mod udp;

use crate::state::State;
use anyhow::{bail, ensure, Context, Result};
use clap::Parser;
use interfaces::Interface;
use ipnet::Ipv4Net;
use macaddr::MacAddr6;
use std::{
    fs,
    io::{BufRead, BufReader},
    net::{IpAddr, Ipv4Addr},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::Arc,
};
use tokio::task::JoinHandle;

/// Finds the mac address for the given ip.
///
/// This function searches the address in the arp cache, if it is not available it tries to
/// populate it by pinging the peer.
fn find_mac(ip: Ipv4Addr) -> Result<MacAddr6> {
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

/// Find the network where the server has the given IP.
fn find_network(ip: Ipv4Addr) -> Result<(String, Ipv4Net)> {
    for interface in Interface::get_all()? {
        for address in &interface.addresses {
            let Some(IpAddr::V4(addr)) = address.addr.map(|x| x.ip()) else {
                continue;
            };
            let Some(IpAddr::V4(mask)) = address.mask.map(|x| x.ip()) else {
                continue;
            };
            let network = Ipv4Net::with_netmask(addr, mask).expect("invalid network mask");
            if addr == ip {
                return Ok((interface.name.clone(), network));
            }
        }
    }
    bail!("Could not find the network for {}", ip);
}

/// Command line arguments for pixie-server.
#[derive(Parser, Debug)]
struct PixieOptions {
    /// Directory in which files will be stored.
    /// Must already contain files: `tftpboot/pixie-uefi.efi` and `config.yaml`.
    #[clap(short, long, default_value = "./storage")]
    storage_dir: PathBuf,
}

#[tokio::main]
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

    let state2 = state.clone();
    tokio::spawn(async move {
        let mut signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
            .expect("failed to register signal handler");
        while let Some(()) = signal.recv().await {
            state2.reload().unwrap();
        }
    });

    async fn flatten(task: JoinHandle<Result<()>>) -> Result<()> {
        task.await??;
        Ok(())
    }

    let dnsmasq_task = flatten(tokio::spawn(dnsmasq::main(state.clone())));
    let http_task = flatten(tokio::spawn(http::main(state.clone())));
    let udp_task = flatten(tokio::spawn(udp::main(state.clone())));
    let tcp_task = flatten(tokio::spawn(tcp::main(state.clone())));
    let ping_task = flatten(tokio::spawn(ping::main(state.clone())));

    tokio::try_join!(dnsmasq_task, http_task, udp_task, tcp_task, ping_task)?;

    Ok(())
}
