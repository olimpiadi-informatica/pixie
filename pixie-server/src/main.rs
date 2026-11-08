mod dhcp;
mod dns;
mod http;
mod ping;
mod state;
mod tcp;
mod tftp;
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
};
use tokio::{signal::unix::SignalKind, task::JoinHandle};

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
/// First checks the in-memory DHCP lease table and registered units, then searches
/// the address in the kernel ARP cache (`/proc/net/arp`).
fn find_mac(state: &State, ip: IpAddr) -> Result<MacAddr6> {
    if ip.is_loopback() {
        bail!("localhost not supported");
    }

    let IpAddr::V4(v4) = ip else {
        bail!("IPv6 not supported");
    };

    state
        .lookup_mac(v4)
        .or_else(|| lookup_proc_arp(v4))
        .context("Mac address not found")
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

    let bootfile = options
        .storage_dir
        .join("tftpboot")
        .join(dhcp::BOOTFILE_NAME);
    ensure!(bootfile.is_file(), "{} not found", bootfile.display());

    let state = Arc::new(State::load(options.storage_dir)?);

    tokio::spawn({
        let state = state.clone();
        let mut sigint = tokio::signal::unix::signal(SignalKind::interrupt())
            .expect("failed to register SIGINT handler");
        let mut sigterm = tokio::signal::unix::signal(SignalKind::terminate())
            .expect("failed to register SIGTERM handler");
        let mut sighup = tokio::signal::unix::signal(SignalKind::hangup())
            .expect("failed to register SIGHUP handler");
        async move {
            let _token_guard = state.cancel_token.clone().drop_guard();
            loop {
                tokio::select! {
                    _ = sigint.recv() => {
                        log::info!("Received SIGINT.");
                        break;
                    }
                    _ = sigterm.recv() => {
                        log::info!("Received SIGTERM.");
                        break;
                    }
                    _ = sighup.recv() => {
                        log::info!("Received SIGHUP, reloading configuration and hostsfile...");
                        if let Err(err) = state.reload() {
                            log::error!("Failed to reload configuration: {err:#}");
                        } else {
                            log::info!("Configuration and hostsfile reloaded successfully.");
                        }
                    }
                }
            }
        }
    });

    async fn flatten(task: JoinHandle<Result<()>>) -> Result<()> {
        task.await??;
        Ok(())
    }

    let dhcp_task = flatten(tokio::spawn(dhcp::main(state.clone())));
    let tftp_task = flatten(tokio::spawn(tftp::main(state.clone())));
    let dns_task = flatten(tokio::spawn(dns::main(state.clone())));
    let http_task = flatten(tokio::spawn(http::main(state.clone())));
    let udp_task = flatten(tokio::spawn(udp::main(state.clone())));
    let tcp_task = flatten(tokio::spawn(tcp::main(state.clone())));
    let ping_task = flatten(tokio::spawn(ping::main(state.clone())));

    tokio::try_join!(
        dhcp_task, tftp_task, dns_task, http_task, udp_task, tcp_task, ping_task
    )?;

    Ok(())
}
