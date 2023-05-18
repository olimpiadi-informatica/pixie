use std::{
    fs::File,
    io::{Error, Write},
    path::Path,
    process::{Child, Command},
    time::Duration,
};

use anyhow::Result;

use pixie_shared::{DhcpConfig, DhcpMode, Unit, UNASSIGNED_GROUP_ID};

pub struct DnsmasqHandle {
    child: Child,
    hosts: File,
}

impl DnsmasqHandle {
    pub fn from_config(storage_dir: &Path, cfg: &DhcpConfig) -> Result<Self> {
        let storage_str = storage_dir.to_str().unwrap();

        let name = &cfg.interface;

        let mut dnsmasq_conf = File::create(storage_dir.join("dnsmasq.conf"))?;
        let hosts = File::create(storage_dir.join("hosts"))?;

        let dhcp_dynamic_conf = match cfg.mode {
            DhcpMode::Static => {
                format!("dhcp-range=10.{UNASSIGNED_GROUP_ID}.0.1,10.{UNASSIGNED_GROUP_ID}.0.100")
            }
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

        Ok(DnsmasqHandle { child, hosts })
    }

    pub fn set_hosts(&mut self, hosts: &Vec<Unit>) -> Result<()> {
        self.hosts.set_len(0)?;
        for host in hosts {
            let mac = host.mac;
            let ip = host.static_ip();
            writeln!(self.hosts, "{},{}", mac, ip)?;
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
