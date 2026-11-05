//! Built-in DHCPv4, PXE, and ProxyDHCP server.

use crate::{find_network, state::State};
use anyhow::{Context, Result};
use dhcproto::{
    Decodable, Decoder, Encodable, Encoder,
    v4::{CLIENT_PORT, DhcpOption, Flags, Message, MessageType, Opcode, OptionCode, SERVER_PORT},
};
use macaddr::MacAddr6;
use pixie_shared::{DhcpMode, InterfaceConfig};
use socket2::{Domain, Protocol, Socket, Type};
use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::{Arc, Mutex},
};
use tokio::net::UdpSocket;

pub const BOOTFILE_NAME: &str = "pixie-client.efi";
const PXE_PORT: u16 = 4011;
const DEFAULT_LEASE_SECS: u32 = 3600;

/// Tracks dynamic IP allocations for unregistered netbooting machines in `DhcpMode::Static(low, high)`.
#[derive(Default)]
pub struct DynamicPool {
    mac_to_ip: HashMap<MacAddr6, Ipv4Addr>,
    ip_to_mac: HashMap<Ipv4Addr, MacAddr6>,
}

impl DynamicPool {
    pub fn get_existing(&self, mac: MacAddr6) -> Option<Ipv4Addr> {
        self.mac_to_ip.get(&mac).copied()
    }

    pub fn allocate(
        &mut self,
        mac: MacAddr6,
        low: Ipv4Addr,
        high: Ipv4Addr,
        is_reserved: impl Fn(Ipv4Addr) -> bool,
    ) -> Option<Ipv4Addr> {
        if let Some(&ip) = self.mac_to_ip.get(&mac) {
            let ip_u32 = u32::from(ip);
            if ip_u32 >= u32::from(low) && ip_u32 <= u32::from(high) {
                return Some(ip);
            }
        }

        let start = u32::from(low);
        let end = u32::from(high);
        for raw in start..=end {
            let candidate = Ipv4Addr::from(raw);
            if !self.ip_to_mac.contains_key(&candidate) && !is_reserved(candidate) {
                self.mac_to_ip.insert(mac, candidate);
                self.ip_to_mac.insert(candidate, mac);
                return Some(candidate);
            }
        }
        None
    }

    pub fn release(&mut self, mac: MacAddr6) {
        if let Some(ip) = self.mac_to_ip.remove(&mac) {
            self.ip_to_mac.remove(&ip);
        }
    }
}

fn bind_interface_udp(port: u16, iface_name: &str) -> Result<UdpSocket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .context("Socket::new(IPV4, DGRAM) failed")?;
    sock.set_reuse_address(true)
        .context("set_reuse_address failed")?;
    sock.set_broadcast(true).context("set_broadcast failed")?;
    sock.bind_device(Some(iface_name.as_bytes()))
        .with_context(|| format!("bind_device({iface_name}) failed"))?;
    sock.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port).into())
        .with_context(|| format!("bind(0.0.0.0:{port}) on {iface_name} failed"))?;
    sock.set_nonblocking(true)
        .context("set_nonblocking failed")?;
    Ok(UdpSocket::from_std(sock.into())?)
}

fn is_netboot_request(req: &Message) -> bool {
    if let Some(DhcpOption::ClassIdentifier(class_id)) = req.opts().get(OptionCode::ClassIdentifier)
        && (class_id.starts_with(b"PXEClient") || class_id.starts_with(b"pixie"))
    {
        return true;
    }
    req.opts()
        .get(OptionCode::ClientSystemArchitecture)
        .is_some()
}

fn pxe_vendor_options() -> Vec<u8> {
    // Suboption 6 (PXE_DISCOVERY_CONTROL) = 8 (disable broadcast/multicast discovery, use bootfile)
    // Suboption 10 (PXE_MENU_PROMPT) = timeout 1s, "pixie\0"
    // Suboption 255 (End)
    vec![6, 1, 8, 10, 7, 1, b'p', b'i', b'x', b'i', b'e', 0, 255]
}

fn encode_message(msg: &Message) -> Option<Vec<u8>> {
    let mut buf = Vec::with_capacity(512);
    let mut enc = Encoder::new(&mut buf);
    msg.encode(&mut enc).ok()?;
    Some(buf)
}

pub fn handle_dhcp_packet(
    state: &State,
    iface: &InterfaceConfig,
    pool: &mut DynamicPool,
    pkt: &[u8],
    peer_addr: SocketAddr,
    is_pxe_service_port: bool,
) -> Option<(Vec<u8>, SocketAddrV4)> {
    let req = Message::decode(&mut Decoder::new(pkt)).ok()?;
    if req.opcode() != Opcode::BootRequest {
        return None;
    }
    let chaddr = req.chaddr();
    if chaddr.len() < 6 {
        return None;
    }
    let mac_bytes: [u8; 6] = chaddr[..6].try_into().ok()?;
    let mac = MacAddr6::from(mac_bytes);

    let msg_type = match req.opts().get(OptionCode::MessageType)? {
        DhcpOption::MessageType(mt) => *mt,
        _ => return None,
    };

    let server_ip = iface.network.addr();
    let is_netboot = is_netboot_request(&req);
    let static_unit = state
        .get_unit(mac)
        .filter(|u| iface.network.contains(&u.static_ip()));

    // Handle ProxyDHCP requests (either on port 4011 or in DhcpMode::Proxy when netbooting)
    let use_proxy_dhcp =
        is_pxe_service_port || (matches!(iface.dhcp, DhcpMode::Proxy(_)) && is_netboot);

    if use_proxy_dhcp {
        let reply_type = match msg_type {
            MessageType::Discover => MessageType::Offer,
            MessageType::Request | MessageType::Inform => MessageType::Ack,
            _ => return None,
        };

        let client_ip = if !req.ciaddr().is_unspecified() {
            Some(req.ciaddr())
        } else if let SocketAddr::V4(v4) = peer_addr
            && !v4.ip().is_unspecified()
        {
            Some(*v4.ip())
        } else if let Some(DhcpOption::RequestedIpAddress(ip)) =
            req.opts().get(OptionCode::RequestedIpAddress)
            && !ip.is_unspecified()
        {
            Some(*ip)
        } else {
            None
        };
        if let Some(ip) = client_ip {
            state.record_dhcp_lease(ip, mac);
        }

        let mut reply = Message::new_with_id(
            req.xid(),
            req.ciaddr(),
            Ipv4Addr::UNSPECIFIED,
            server_ip,
            req.giaddr(),
            chaddr,
        );
        reply
            .set_opcode(Opcode::BootReply)
            .set_flags(req.flags())
            .set_sname_str(server_ip.to_string())
            .set_fname_str(BOOTFILE_NAME);

        reply.opts_mut().insert(DhcpOption::MessageType(reply_type));
        reply
            .opts_mut()
            .insert(DhcpOption::ServerIdentifier(server_ip));
        reply
            .opts_mut()
            .insert(DhcpOption::ClassIdentifier(b"PXEClient".to_vec()));
        reply.opts_mut().insert(DhcpOption::TFTPServerName(
            server_ip.to_string().into_bytes(),
        ));
        reply
            .opts_mut()
            .insert(DhcpOption::BootfileName(BOOTFILE_NAME.as_bytes().to_vec()));
        reply
            .opts_mut()
            .insert(DhcpOption::VendorExtensions(pxe_vendor_options()));
        if let Some(uid) = req.opts().get(OptionCode::ClientMachineIdentifier).cloned() {
            reply.opts_mut().insert(uid);
        }

        let dst = if is_pxe_service_port {
            match peer_addr {
                SocketAddr::V4(v4) if !v4.ip().is_unspecified() => v4,
                _ => SocketAddrV4::new(Ipv4Addr::BROADCAST, CLIENT_PORT),
            }
        } else if !req.ciaddr().is_unspecified() && !req.flags().broadcast() {
            SocketAddrV4::new(req.ciaddr(), CLIENT_PORT)
        } else {
            SocketAddrV4::new(Ipv4Addr::BROADCAST, CLIENT_PORT)
        };

        return Some((encode_message(&reply)?, dst));
    }

    // Standard DHCPv4 assignment (static unit IP or dynamic range)
    if matches!(msg_type, MessageType::Release | MessageType::Decline) {
        pool.release(mac);
        return None;
    }

    let assigned_ip = if let Some(unit) = static_unit {
        unit.static_ip()
    } else if let DhcpMode::Static(low, high) = iface.dhcp {
        if is_netboot || pool.get_existing(mac).is_some() {
            pool.allocate(mac, low, high, |ip| state.lookup_mac(ip).is_some())?
        } else {
            return None;
        }
    } else {
        return None;
    };

    let reply_type = match msg_type {
        MessageType::Discover => MessageType::Offer,
        MessageType::Request => {
            if let Some(DhcpOption::ServerIdentifier(sid)) =
                req.opts().get(OptionCode::ServerIdentifier)
                && *sid != server_ip
            {
                return None;
            }
            let req_ip = match req.opts().get(OptionCode::RequestedIpAddress) {
                Some(DhcpOption::RequestedIpAddress(ip)) => Some(*ip),
                _ if !req.ciaddr().is_unspecified() => Some(req.ciaddr()),
                _ => None,
            };
            if let Some(req_ip) = req_ip
                && req_ip != assigned_ip
            {
                let mut nak = Message::new_with_id(
                    req.xid(),
                    Ipv4Addr::UNSPECIFIED,
                    Ipv4Addr::UNSPECIFIED,
                    server_ip,
                    req.giaddr(),
                    chaddr,
                );
                nak.set_opcode(Opcode::BootReply)
                    .set_flags(Flags::default().set_broadcast());
                nak.opts_mut()
                    .insert(DhcpOption::MessageType(MessageType::Nak));
                nak.opts_mut()
                    .insert(DhcpOption::ServerIdentifier(server_ip));
                return Some((
                    encode_message(&nak)?,
                    SocketAddrV4::new(Ipv4Addr::BROADCAST, CLIENT_PORT),
                ));
            }
            state.record_dhcp_lease(assigned_ip, mac);
            MessageType::Ack
        }
        _ => return None,
    };

    if reply_type == MessageType::Offer {
        state.record_dhcp_lease(assigned_ip, mac);
    }

    let mut reply = Message::new_with_id(
        req.xid(),
        req.ciaddr(),
        assigned_ip,
        server_ip,
        req.giaddr(),
        chaddr,
    );
    reply
        .set_opcode(Opcode::BootReply)
        .set_flags(req.flags())
        .set_sname_str(server_ip.to_string())
        .set_fname_str(BOOTFILE_NAME);

    reply.opts_mut().insert(DhcpOption::MessageType(reply_type));
    reply
        .opts_mut()
        .insert(DhcpOption::ServerIdentifier(server_ip));
    reply
        .opts_mut()
        .insert(DhcpOption::AddressLeaseTime(DEFAULT_LEASE_SECS));
    reply
        .opts_mut()
        .insert(DhcpOption::SubnetMask(iface.network.netmask()));
    reply
        .opts_mut()
        .insert(DhcpOption::BroadcastAddr(iface.network.broadcast()));
    reply.opts_mut().insert(DhcpOption::Router(vec![server_ip]));
    reply
        .opts_mut()
        .insert(DhcpOption::DomainNameServer(vec![server_ip]));
    reply.opts_mut().insert(DhcpOption::TFTPServerName(
        server_ip.to_string().into_bytes(),
    ));
    reply
        .opts_mut()
        .insert(DhcpOption::BootfileName(BOOTFILE_NAME.as_bytes().to_vec()));

    if let Some(hostname) = state.hostmap().get(&assigned_ip) {
        reply
            .opts_mut()
            .insert(DhcpOption::Hostname(hostname.clone()));
    }

    let dst = if !req.giaddr().is_unspecified() {
        SocketAddrV4::new(req.giaddr(), SERVER_PORT)
    } else if !req.ciaddr().is_unspecified() && !req.flags().broadcast() {
        SocketAddrV4::new(req.ciaddr(), CLIENT_PORT)
    } else {
        SocketAddrV4::new(Ipv4Addr::BROADCAST, CLIENT_PORT)
    };

    log::debug!(
        "DHCP {:?} -> {:?} for {} (yiaddr={assigned_ip}, netboot={is_netboot})",
        msg_type,
        reply_type,
        mac
    );

    Some((encode_message(&reply)?, dst))
}

async fn serve_socket(
    state: Arc<State>,
    iface: InterfaceConfig,
    pool: Arc<Mutex<DynamicPool>>,
    socket: UdpSocket,
    is_pxe_service_port: bool,
) -> Result<()> {
    let mut buf = [0u8; 2048];
    loop {
        let (len, peer_addr) = tokio::select! {
            res = socket.recv_from(&mut buf) => res?,
            _ = state.cancel_token.cancelled() => break,
        };

        let response = {
            let mut pool_guard = pool.lock().expect("DHCP pool mutex poisoned");
            handle_dhcp_packet(
                &state,
                &iface,
                &mut pool_guard,
                &buf[..len],
                peer_addr,
                is_pxe_service_port,
            )
        };

        if let Some((reply_bytes, dst)) = response
            && let Err(e) = socket.send_to(&reply_bytes, dst).await
        {
            log::warn!("Failed to send DHCP reply to {dst}: {e}");
        }
    }
    Ok(())
}

pub async fn main(state: Arc<State>) -> Result<()> {
    let mut tasks = Vec::new();
    let interfaces = state.config().hosts.interfaces.clone();

    for iface in &interfaces {
        let (iface_name, _) = find_network(iface.network.addr())?;
        let pool = Arc::new(Mutex::new(DynamicPool::default()));

        let dhcp_sock = bind_interface_udp(SERVER_PORT, &iface_name)?;
        log::info!(
            "DHCP listening on {}:{} ({})",
            iface_name,
            SERVER_PORT,
            iface.network
        );
        tasks.push(tokio::spawn(serve_socket(
            state.clone(),
            iface.clone(),
            pool.clone(),
            dhcp_sock,
            false,
        )));

        if matches!(iface.dhcp, DhcpMode::Proxy(_)) {
            let pxe_sock = bind_interface_udp(PXE_PORT, &iface_name)?;
            log::info!("ProxyDHCP PXE service listening on {iface_name}:{PXE_PORT}");
            tasks.push(tokio::spawn(serve_socket(
                state.clone(),
                iface.clone(),
                pool,
                pxe_sock,
                true,
            )));
        }
    }

    for task in tasks {
        task.await??;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ipnet::Ipv4Net;
    use pixie_shared::{
        Action, Bijection, Config, HostsConfig, HttpConfig, RegistrationInfo, UnitStats,
    };

    fn dummy_state(dhcp_mode: DhcpMode) -> (tempfile::TempDir, Arc<State>) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("chunks")).unwrap();
        std::fs::create_dir_all(dir.path().join("images")).unwrap();
        std::fs::create_dir_all(dir.path().join("tftpboot")).unwrap();

        let groups: Bijection<String, u8> = serde_yaml::from_str("- [room0, 0]\n").unwrap();

        let config = Config {
            hosts: HostsConfig {
                interfaces: vec![InterfaceConfig {
                    network: "10.0.0.1/16".parse::<Ipv4Net>().unwrap(),
                    dhcp: dhcp_mode,
                    broadcast_speed: 50_000_000,
                }],
                hostsfile: None,
                dns_upstream: false,
            },
            http: HttpConfig {
                listen_on: "127.0.0.1:0".to_string(),
            },
            groups,
            images: vec!["contestant".to_string()],
        };

        std::fs::write(
            dir.path().join("config.yaml"),
            serde_yaml::to_string(&config).unwrap(),
        )
        .unwrap();

        let state = Arc::new(State::load(dir.path().to_path_buf()).unwrap());
        (dir, state)
    }

    #[tokio::test]
    async fn test_dhcp_registered_unit_and_dynamic_netboot() {
        let (_dir, state) = dummy_state(DhcpMode::Static(
            Ipv4Addr::new(10, 0, 100, 1),
            Ipv4Addr::new(10, 0, 100, 10),
        ));
        let iface = state.config().hosts.interfaces[0].clone();
        let iface = &iface;
        let mut pool = DynamicPool::default();

        // Register a unit at group 0, row 2, col 3 -> 10.0.2.3
        let reg_mac = MacAddr6::new(0x52, 0x54, 0x00, 0x12, 0x34, 0x56);
        state
            .register_unit(
                reg_mac,
                RegistrationInfo {
                    group: "room0".to_string(),
                    row: 2,
                    col: 3,
                    image: "contestant".to_string(),
                },
                UnitStats::default(),
            )
            .unwrap();
        state.set_unit_next_action(crate::state::UnitSelector::MacAddr(reg_mac), Action::Wait);

        // 1. Registered unit sends DISCOVER -> should receive OFFER with 10.0.2.3
        let mut discover = Message::new(
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::UNSPECIFIED,
            reg_mac.as_bytes(),
        );
        discover
            .opts_mut()
            .insert(DhcpOption::MessageType(MessageType::Discover));
        let pkt = encode_message(&discover).unwrap();

        let (reply_bytes, _) = handle_dhcp_packet(
            &state,
            iface,
            &mut pool,
            &pkt,
            "0.0.0.0:68".parse().unwrap(),
            false,
        )
        .expect("Expected DHCPOFFER for registered unit");
        let offer = Message::decode(&mut Decoder::new(&reply_bytes)).unwrap();
        assert_eq!(offer.yiaddr(), Ipv4Addr::new(10, 0, 2, 3));
        assert_eq!(
            offer.fname_str().unwrap().unwrap().trim_end_matches('\0'),
            BOOTFILE_NAME
        );

        // 2. Unregistered PXE client sends DISCOVER -> should receive OFFER from dynamic pool (10.0.100.1)
        let unreg_mac = [0x52, 0x54, 0x00, 0x99, 0x88, 0x77];
        let mut pxe_discover = Message::new(
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::UNSPECIFIED,
            &unreg_mac,
        );
        pxe_discover
            .opts_mut()
            .insert(DhcpOption::MessageType(MessageType::Discover));
        pxe_discover.opts_mut().insert(DhcpOption::ClassIdentifier(
            b"PXEClient:Arch:00007:UNDI:003016".to_vec(),
        ));
        let pxe_pkt = encode_message(&pxe_discover).unwrap();

        let (pxe_reply, _) = handle_dhcp_packet(
            &state,
            iface,
            &mut pool,
            &pxe_pkt,
            "0.0.0.0:68".parse().unwrap(),
            false,
        )
        .expect("Expected DHCPOFFER for unregistered PXE client");
        let pxe_offer = Message::decode(&mut Decoder::new(&pxe_reply)).unwrap();
        assert_eq!(pxe_offer.yiaddr(), Ipv4Addr::new(10, 0, 100, 1));
    }

    #[tokio::test]
    async fn test_dhcp_proxy_mode() {
        let (_dir, state) = dummy_state(DhcpMode::Proxy(Ipv4Addr::new(10, 0, 0, 1)));
        let iface = state.config().hosts.interfaces[0].clone();
        let iface = &iface;
        let mut pool = DynamicPool::default();

        let unreg_mac = [0x52, 0x54, 0x00, 0xAA, 0xBB, 0xCC];
        let mut pxe_discover = Message::new(
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::UNSPECIFIED,
            &unreg_mac,
        );
        pxe_discover
            .opts_mut()
            .insert(DhcpOption::MessageType(MessageType::Discover));
        pxe_discover.opts_mut().insert(DhcpOption::ClassIdentifier(
            b"PXEClient:Arch:00007:UNDI:003016".to_vec(),
        ));
        let pxe_pkt = encode_message(&pxe_discover).unwrap();

        let (proxy_reply, _) = handle_dhcp_packet(
            &state,
            iface,
            &mut pool,
            &pxe_pkt,
            "0.0.0.0:68".parse().unwrap(),
            false,
        )
        .expect("Expected ProxyDHCP OFFER");
        let proxy_offer = Message::decode(&mut Decoder::new(&proxy_reply)).unwrap();
        assert_eq!(proxy_offer.yiaddr(), Ipv4Addr::UNSPECIFIED);
        assert_eq!(proxy_offer.siaddr(), Ipv4Addr::new(10, 0, 0, 1));
        assert_eq!(
            proxy_offer
                .fname_str()
                .unwrap()
                .unwrap()
                .trim_end_matches('\0'),
            BOOTFILE_NAME
        );
    }
}
