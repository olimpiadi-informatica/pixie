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

use anyhow::{bail, ensure, Result};
use interfaces::HardwareAddr;
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
    /// IP range for dhcp server
    pub netaddr: Option<Ipv4Net>,
    /// Address for proxy-dhcp
    pub proxy: Option<Ipv4Addr>,
    /// Name of the interface this network is served on.
    pub interface: String,
}

#[derive(Debug, Eq, PartialEq, Deserialize)]
pub struct Config {
    pub networks: Vec<Net>,
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

        let name = &net.interface;
        let netid = 0;
        let server_port = http_cfg.listen_port;

        let mut dnsmasq_conf = File::create(storage_dir.join("dnsmasq.conf"))?;
        let hosts = File::create(storage_dir.join("hosts"))?;

        let (ip, magic_line) = match (net.netaddr, net.proxy) {
            (Some(net), None) => {
                let begin = net.network();
                let end = net.broadcast();
                (
                    net.addr(),
                    format!("dhcp-range=set:net{netid},{begin},{end}"),
                )
            }
            (None, Some(ip)) => (ip, format!("dhcp-range=set:net{netid},{ip},proxy")),
            _ => bail!("specify exactly one between netaddr or proxy"),
        };

        write!(
            dnsmasq_conf,
            r#"
### Per-network configuration

## net0
{magic_line}
dhcp-range=set:net{netid},10.0.0.0,static
dhcp-hostsfile={storage_str}/hosts
dhcp-boot=tag:pxe,tag:net{netid},ipxe.efi,,{ip}
dhcp-boot=tag:ipxe,tag:net{netid},http://{ip}:{server_port}/boot.ipxe
interface={name}
except-interface=lo
user=root
group=root
bind-interfaces

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
