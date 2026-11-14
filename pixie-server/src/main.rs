mod dnsmasq;
mod http;
mod ping;
mod state;
mod tcp;
mod udp;

use crate::state::State;
use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use interfaces::Interface;
use ipnet::Ipv4Net;
use macaddr::MacAddr6;
use std::{
    fs,
    net::{IpAddr, Ipv4Addr},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::{net::UdpSocket, signal::unix::SignalKind, task::JoinHandle, time};

fn lookup_proc_arp(ip: Ipv4Addr) -> Option<MacAddr6> {
    let content = fs::read_to_string("/proc/net/arp").ok()?;
    for line in content.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let Some(Ok(entry_ip)) = fields.next().map(str::parse::<Ipv4Addr>) else {
            continue;
        };
        let _hw_type = fields.next()?;
        let flags = fields.next()?;
        let hw_addr = fields.next()?;
        if entry_ip == ip
            && flags != "0x0"
            && let Ok(mac) = hw_addr.parse::<MacAddr6>()
            && !mac.is_nil()
        {
            return Some(mac);
        }
    }
    None
}

/// Finds the mac address for the given ip.
///
/// Searches `/proc/net/arp`, triggering kernel ARP resolution via a dummy UDP probe on miss.
async fn find_mac(ip: IpAddr) -> Result<MacAddr6> {
    if ip.is_loopback() {
        bail!("localhost not supported");
    }

    let IpAddr::V4(v4) = ip else {
        bail!("IPv6 not supported");
    };

    if let Some(mac) = lookup_proc_arp(v4) {
        return Ok(mac);
    }

    // Trigger kernel ARP resolution on cold cache.
    if let Ok(sock) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await {
        let _ = sock.send_to(&[], (v4, 9)).await;
        for delay_ms in [10, 20, 30, 40] {
            time::sleep(Duration::from_millis(delay_ms)).await;
            if let Some(mac) = lookup_proc_arp(v4) {
                return Ok(mac);
            }
        }
    }

    bail!("Mac address not found for {v4}")
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

    tokio::spawn({
        let cancel_token = state.cancel_token.clone();
        let mut sigint = tokio::signal::unix::signal(SignalKind::interrupt())
            .expect("failed to register SIGNINT handler");
        let mut sigterm = tokio::signal::unix::signal(SignalKind::terminate())
            .expect("failed to register SIGNTERM handler");
        async move {
            let _token_guard = cancel_token.drop_guard();
            tokio::select! {
                _ = sigint.recv() => log::info!("Received SIGINT."),
                _ = sigterm.recv() => log::info!("Received SIGTERM."),
            };
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
