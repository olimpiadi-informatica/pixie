use std::{
    collections::HashMap,
    fs::File,
    io::{Error, Seek, SeekFrom, Write},
    net::Ipv4Addr,
    ops::Range,
    path::Path,
    process::{Child, Command},
    thread,
    time::Duration,
};

use anyhow::{ensure, Context, Result};
use interfaces::{HardwareAddr, Interface};
use ipnet::Ipv4Net;
use macaddr::MacAddr6;
use serde_derive::Deserialize;

use crate::http;

#[derive(Debug, Eq, PartialEq)]
pub struct ClientInfo {
    pub ip: Ipv4Addr,
    pub hostname: String,
}

#[derive(Debug, Eq, PartialEq)]
pub struct FixedNet {
    /// IP address to use for this network.
    pub ip: Ipv4Net,
    /// Automatic assignment of IP addresses to unknown clients.
    pub dhcp_range: Range<Ipv4Addr>,
    /// Known clients in this subnet.
    pub clients: HashMap<HardwareAddr, ClientInfo>,
    /// Hostname to expose over DNS for the IP of the server in this net.
    pub hostname: String,
}

#[derive(Debug, Eq, PartialEq, Deserialize)]
pub struct Net {
    /// If None, this represents a proxy-dhcp subnet; the server IP will be deduced by the
    /// first available address on the specified interface.
    pub dhcp_config: Option<Ipv4Net>,
    /// Name of the interface this network is served on.
    pub interface: String,
}

#[derive(Debug, Eq, PartialEq, Deserialize)]
pub struct Config {
    pub networks: Vec<Net>,
}

impl Config {
    pub fn to_dnsmasq_config(
        &self,
        storage_dir: &Path,
        http_config: &crate::http::Config,
    ) -> Result<String> {
        ensure!(self.networks.len() == 1, "Not implemented: >1 network");

        let net = &self.networks[0];

        anyhow::ensure!(
            net.dhcp_config.is_none(),
            "Not implemented: non-dhcp-proxy interfaces"
        );

        // Get an IPv4 address on the chosen interface.
        let name = &net.interface;
        let interface = Interface::get_by_name(name).context("Error listing interfaces")?;
        let interface = interface.with_context(|| format!("Unknown interface: {}", name))?;

        let ip = interface
            .addresses
            .iter()
            .find(|x| x.kind == interfaces::Kind::Ipv4)
            .with_context(|| format!("Interface {} has no ipv4 address", name))?;

        let ip = ip.addr.unwrap();

        let ip = match ip {
            std::net::SocketAddr::V4(addr) => *addr.ip(),
            _ => panic!("IPv4 address is not IPv4"),
        };

        let tftp_root = storage_dir.join("tftpboot").to_str().unwrap().to_owned();

        let netid = 0;
        let server_port = http_config.listen_port;

        Ok(format!(
            r#"
### Per-network configuration

## net0
dhcp-range=set:net{netid},{ip},proxy
dhcp-boot=tag:pxe,tag:net{netid},ipxe.efi,,{ip}
dhcp-boot=tag:ipxe,tag:net{netid},http://{ip}:{server_port}/boot.ipxe
interface={name}

### Common configuration

## Root for TFTP server
tftp-root={tftp_root}
enable-tftp

## PXE prompt and timeout
pxe-prompt="pixie",1

## PXE kind recognition
# BC_UEFI (00007)
dhcp-vendorclass=set:pxe,PXEClient:Arch:00007
# UEFI x86-64 (00009)
dhcp-vendorclass=set:pxe,PXEClient:Arch:00009
# iPXE
dhcp-userclass=set:ipxe,iPXE
"#
        ))
    }
}

pub struct DnsmasqHandle {
    child: Child,
    hosts: File,
}

impl DnsmasqHandle {
    pub fn from_config(storage_dir: &Path, cfg: &Config, http_cfg: &http::Config) -> Result<Self> {
        let storage_str = storage_dir.to_str().unwrap();

        ensure!(cfg.networks.len() == 1, "Not implemented: >1 network");
        let net = &cfg.networks[0];

        let netmask = match net.dhcp_config {
            Some(netmask) => netmask,
            None => todo!("dhcp-proxy is not suported"),
        };

        let name = &net.interface;
        let ip = netmask.addr();

        let netid = 0;
        let server_port = http_cfg.listen_port;

        let mut dnsmasq_conf = File::create(storage_dir.join("dnsmasq.conf"))?;
        let hosts = File::create(storage_dir.join("hosts"))?;

        let netmask_network = netmask.network();
        let netmask_broadcast = netmask.broadcast();

        write!(
            dnsmasq_conf,
            r#"
### Per-network configuration

## net0
dhcp-range=set:net{netid},{netmask_network},{netmask_broadcast}
# TODO: dhcp-range=set:net{netid},{ip},proxy
dhcp-range=set:net{netid},10.0.0.0,static
dhcp-hostsfile={storage_str}/hosts
dhcp-boot=tag:pxe,tag:net{netid},ipxe.efi,,{ip}
dhcp-boot=tag:ipxe,tag:net{netid},http://{ip}:{server_port}/boot.ipxe
interface={name}

### Common configuration

## Root for TFTP server
tftp-root={storage_str}/tftpboot
enable-tftp

## PXE prompt and timeout
pxe-prompt="pixie",1

## PXE kind recognition
# BC_UEFI (00007)
dhcp-vendorclass=set:pxe,PXEClient:Arch:00007
# UEFI x86-64 (00009)
dhcp-vendorclass=set:pxe,PXEClient:Arch:00009
# iPXE
dhcp-userclass=set:ipxe,iPXE
"#
        )?;

        let mut child = Command::new("dnsmasq")
            .arg(format!("--conf-file={storage_str}/dnsmasq.conf"))
            .arg("--log-dhcp")
            .arg("--no-daemon")
            .spawn()?;

        // TODO: better check
        // removing the sleep causes dnsmasq to die
        thread::sleep(Duration::from_secs(1));
        assert!(child.try_wait()?.is_none());

        Ok(DnsmasqHandle { child, hosts })
    }

    pub fn write_host(&mut self, idx: usize, mac: MacAddr6, ip: Ipv4Addr) -> Result<()> {
        let size = 3 * 6 + 4 * 4;
        self.hosts.seek(SeekFrom::Start((idx * size) as u64))?;
        writeln!(self.hosts, "{},{:15}", mac, ip)?;
        Ok(())
    }

    pub fn send_sighup(&mut self) -> Result<()> {
        let r = unsafe { libc::kill(self.child.id().try_into().unwrap(), libc::SIGHUP) };
        if r < 0 {
            return Err(Error::last_os_error().into());
        }
        Ok(())
    }
}

impl Drop for DnsmasqHandle {
    fn drop(&mut self) {
        self.child.kill().unwrap();
    }
}
