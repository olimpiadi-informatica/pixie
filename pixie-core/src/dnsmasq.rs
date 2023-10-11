use std::{
    collections::HashMap,
    fs::File,
    io::{Error, Write},
    net::{IpAddr, Ipv4Addr},
    path::Path,
    process::{Child, Command},
    time::Duration,
};

use anyhow::{bail, Result};

use pixie_shared::{DhcpConfig, DhcpMode, Unit};

pub struct DnsmasqHandle {
    child: Child,
    hosts: File,
    pub hostmap: HashMap<Ipv4Addr, String>,
}

impl DnsmasqHandle {
    pub fn from_config(storage_dir: &Path, cfg: &DhcpConfig) -> Result<Self> {
        let storage_str = storage_dir.to_str().unwrap();

        let name = &cfg.interface;

        let mut dnsmasq_conf = File::create(storage_dir.join("dnsmasq.conf"))?;
        let hosts = File::create(storage_dir.join("hosts"))?;

        let dhcp_dynamic_conf = match cfg.mode {
            DhcpMode::Static(low, high) => format!("dhcp-range={low},{high}"),
            DhcpMode::Proxy(ip) => format!("dhcp-range={},proxy", ip),
        };

        write!(
            dnsmasq_conf,
            r#"
### Per-network configuration

## net0
{dhcp_dynamic_conf}
dhcp-range=10.0.0.0,static
dhcp-hostsfile={storage_str}/hosts
dhcp-boot=uefi_app.efi
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
"#
        )?;

        let mut child = Command::new("dnsmasq")
            .arg(format!("--conf-file={storage_str}/dnsmasq.conf"))
            .arg("--log-dhcp")
            .arg("--no-daemon")
            .spawn()?;

        // Without this sleep line, dnsmasq does not produce any output.
        std::thread::sleep(Duration::from_secs(1));
        assert!(child.try_wait()?.is_none());

        let mut hostmap = HashMap::new();

        if let Some(hostsfile) = &cfg.hostsfile {
            match hostfile::parse_file(hostsfile) {
                Ok(hosts) => {
                    for host in hosts {
                        if let IpAddr::V4(ip) = host.ip {
                            hostmap.insert(ip, host.names[0].clone());
                        }
                    }
                }
                Err(err) => {
                    bail!("Error parsing host file: {err}");
                }
            }
        }

        Ok(DnsmasqHandle {
            child,
            hosts,
            hostmap,
        })
    }

    pub fn set_hosts(&mut self, hosts: &Vec<Unit>) -> Result<()> {
        self.hosts.set_len(0)?;
        for host in hosts {
            let mac = host.mac;
            let ip = host.static_ip();
            if let Some(hostname) = self.hostmap.get(&ip) {
                writeln!(self.hosts, "{},{},{}", mac, ip, hostname)?;
            } else {
                writeln!(self.hosts, "{},{}", mac, ip)?;
            }
        }
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
