use anyhow::{ensure, Context, Result};
use interfaces::{HardwareAddr, Interface};
use ipnet::Ipv4Net;
use std::{collections::HashMap, net::Ipv4Addr, ops::Range, path::Path};

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

#[derive(Debug, Eq, PartialEq)]
pub struct Net {
    /// If None, this represents a proxy-dhcp subnet; the server IP will be deduced by the
    /// first available address on the specified interface.
    pub dhcp_config: Option<Ipv4Net>,
    /// Name of the interface this network is served on.
    pub interface: String,
}

#[derive(Debug, Eq, PartialEq)]
pub struct Config {
    pub networks: Vec<Net>,
}

impl Config {
    pub fn to_dnsmasq_config(&self, storage_dir: &Path) -> Result<String> {
        ensure!(self.networks.len() == 1, "Not implemented: >1 network");

        let net = &self.networks[0];

        ensure!(
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

        Ok(format!(
            r#"
### Per-network configuration

## net0
dhcp-range=set:net{netid},{ip},proxy
dhcp-boot=tag:pxe,tag:net{netid},ipxe.efi,,{ip}
dhcp-boot=tag:ipxe,tag:net{netid},http://{ip}/boot.ipxe
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
