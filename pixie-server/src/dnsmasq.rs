use crate::{find_network, state::State};
use anyhow::{Context, Result};
use macaddr::MacAddr6;
use pixie_shared::{DhcpMode, Unit};
use std::{
    collections::HashMap,
    fs::File,
    io::{BufWriter, Error, Write},
    net::Ipv4Addr,
    process::{Child, Command},
    sync::Arc,
};

struct DnsmasqHandle {
    child: Child,
}

impl DnsmasqHandle {
    fn reload(&self) -> Result<()> {
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
        self.child.wait().unwrap();
    }
}

async fn write_config(state: &State) -> Result<()> {
    let (name, _) = find_network(state.config.hosts.listen_on)?;

    let mut dnsmasq_conf = File::create(state.storage_dir.join("dnsmasq.conf"))?;

    let dhcp_dynamic_conf = match state.config.hosts.dhcp {
        DhcpMode::Static(low, high) => format!("dhcp-range=tag:netboot,{low},{high}"),
        DhcpMode::Proxy(ip) => format!("dhcp-range=tag:netboot,{},proxy", ip),
    };

    let storage_str = state.storage_dir.to_str().unwrap();

    write!(
        dnsmasq_conf,
        r#"
### Per-network configuration

## net0
{dhcp_dynamic_conf}
dhcp-range=tag:!netboot,10.0.0.0,static,255.0.0.0
dhcp-hostsfile={storage_str}/hosts
dhcp-boot=pixie-uefi.efi
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

dhcp-vendorclass=set:netboot,PXEClient:Arch:00000
dhcp-vendorclass=set:netboot,PXEClient:Arch:00006
dhcp-vendorclass=set:netboot,PXEClient:Arch:00007
dhcp-vendorclass=set:netboot,PXEClient:Arch:00009
dhcp-vendorclass=set:netboot,pixie
"#,
    )?;

    Ok(())
}

async fn write_hosts(state: &State, hosts: &[(MacAddr6, Ipv4Addr, Option<String>)]) -> Result<()> {
    let file = File::create(state.storage_dir.join("hosts"))?;
    let mut file = BufWriter::new(file);

    for (mac, ip, hostname) in hosts {
        if let Some(hostname) = hostname {
            writeln!(file, "{},{},{}", mac, ip, hostname)?;
        } else {
            writeln!(file, "{},{}", mac, ip)?;
        }
    }
    Ok(())
}

fn get_hosts(
    hostmap: &HashMap<Ipv4Addr, String>,
    units: &[Unit],
) -> Vec<(MacAddr6, Ipv4Addr, Option<String>)> {
    units
        .iter()
        .map(|unit| {
            let mac = unit.mac;
            let ip = unit.static_ip();
            let hostname = hostmap.get(&ip).cloned();
            (mac, ip, hostname)
        })
        .collect()
}

pub async fn main(state: Arc<State>) -> Result<()> {
    let mut units_rx = state.subscribe_units();
    let mut hostmap_rx = state.subscribe_hostmap();

    write_config(&state).await?;
    let mut hosts = get_hosts(
        &hostmap_rx.borrow_and_update(),
        &units_rx.borrow_and_update(),
    );
    write_hosts(&state, &hosts).await?;

    let dnsmasq = DnsmasqHandle {
        child: Command::new("dnsmasq")
            .arg(format!(
                "--conf-file={storage_str}/dnsmasq.conf",
                storage_str = state.storage_dir.to_str().unwrap()
            ))
            .arg("--log-dhcp")
            .arg("--no-daemon")
            .spawn()
            .context("Failed to start dnsmasq")?,
    };

    loop {
        tokio::select! {
            ret = units_rx.changed() => ret.unwrap(),
            ret = hostmap_rx.changed() => ret.unwrap(),
        }

        let hosts2 = get_hosts(
            &hostmap_rx.borrow_and_update(),
            &units_rx.borrow_and_update(),
        );
        if hosts != hosts2 {
            hosts = hosts2;
            write_hosts(&state, &hosts).await?;
            dnsmasq.reload()?;
        }
    }
}
