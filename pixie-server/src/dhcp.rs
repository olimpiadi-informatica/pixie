//! Built-in DHCPv4, PXE, and ProxyDHCP server.

use crate::{find_network, state::State};
use anyhow::{Context, Result, ensure};
use dhcproto::{
    Decodable, Encodable,
    v4::{CLIENT_PORT, DhcpOption, Flags, Message, MessageType, Opcode, OptionCode, SERVER_PORT},
};
use macaddr::MacAddr6;
use pixie_shared::{DhcpMode, InterfaceConfig};
use socket2::{Domain, Protocol, Socket, Type};
use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
};
use tokio::net::UdpSocket;

pub const BOOTFILE_NAME: &str = "pixie-uefi.efi";
const PXE_PORT: u16 = 4011;
const DEFAULT_LEASE_SECS: u32 = 3600;

// Suboption 6 (PXE_DISCOVERY_CONTROL) = 8 (disable broadcast/multicast discovery, use bootfile)
// Suboption 10 (PXE_MENU_PROMPT) = timeout 1s, "pixie\0"
// Suboption 255 (End)
const PXE_VENDOR_OPTIONS: &[u8] = &[6, 1, 8, 10, 7, 1, b'p', b'i', b'x', b'i', b'e', 0, 255];

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

fn is_pxe_client(req: &Message) -> bool {
    if let Some(DhcpOption::ClassIdentifier(class_id)) = req.opts().get(OptionCode::ClassIdentifier)
        && class_id.starts_with(b"PXEClient")
    {
        return true;
    }
    req.opts().contains(OptionCode::ClientSystemArchitecture)
}

fn is_netboot_request(req: &Message) -> bool {
    if let Some(DhcpOption::ClassIdentifier(class_id)) = req.opts().get(OptionCode::ClassIdentifier)
        && class_id.starts_with(b"pixie")
    {
        return true;
    }
    is_pxe_client(req)
}

fn build_base_reply(
    req: &Message,
    yiaddr: Ipv4Addr,
    server_ip: Ipv4Addr,
    reply_type: MessageType,
) -> Message {
    let mut reply = Message::new_with_id(
        req.xid(),
        req.ciaddr(),
        yiaddr,
        server_ip,
        req.giaddr(),
        req.chaddr(),
    );
    reply
        .set_opcode(Opcode::BootReply)
        .set_flags(req.flags())
        .set_sname_str(server_ip.to_string())
        .set_fname_str(BOOTFILE_NAME);

    let opts = reply.opts_mut();
    opts.insert(DhcpOption::MessageType(reply_type));
    opts.insert(DhcpOption::ServerIdentifier(server_ip));
    opts.insert(DhcpOption::TFTPServerName(
        server_ip.to_string().into_bytes(),
    ));
    opts.insert(DhcpOption::BootfileName(BOOTFILE_NAME.as_bytes().to_vec()));
    reply
}

fn handle_dhcp_packet(
    state: &State,
    iface: &InterfaceConfig,
    pkt: &[u8],
    peer_addr: SocketAddr,
    is_pxe_service_port: bool,
) -> Option<(Vec<u8>, SocketAddrV4)> {
    let req = Message::from_bytes(pkt).ok()?;
    if req.opcode() != Opcode::BootRequest {
        return None;
    }
    let chaddr = req.chaddr();
    let mac = MacAddr6::from(<[u8; 6]>::try_from(chaddr.get(..6)?).ok()?);
    let msg_type = req.opts().msg_type()?;

    let static_unit = state
        .get_unit(mac)
        .filter(|u| iface.network.contains(&u.static_ip()));
    let req_ip = match req.opts().get(OptionCode::RequestedIpAddress) {
        Some(DhcpOption::RequestedIpAddress(ip)) => Some(*ip),
        _ if !req.ciaddr().is_unspecified() => Some(req.ciaddr()),
        _ => None,
    };

    let use_proxy_dhcp = is_pxe_service_port
        || (matches!(iface.dhcp, DhcpMode::Proxy(_)) && is_netboot_request(&req));

    let reply = if use_proxy_dhcp {
        let proxy_ip = match iface.dhcp {
            DhcpMode::Proxy(ip) => ip,
            DhcpMode::Static(..) => iface.network.addr(),
        };

        let peer_ip = match peer_addr {
            SocketAddr::V4(v4) => Some(*v4.ip()),
            _ => None,
        };
        if let Some(ip) = req_ip.or(peer_ip).filter(|ip| !ip.is_unspecified()) {
            state.record_dhcp_lease(ip, mac);
        }

        if !is_pxe_service_port && !is_pxe_client(&req) {
            return None;
        }

        let reply_type = match msg_type {
            MessageType::Discover => MessageType::Offer,
            MessageType::Request | MessageType::Inform => MessageType::Ack,
            _ => return None,
        };

        let mut reply = build_base_reply(&req, Ipv4Addr::UNSPECIFIED, proxy_ip, reply_type);
        let opts = reply.opts_mut();
        opts.insert(DhcpOption::ClassIdentifier(b"PXEClient".to_vec()));
        opts.insert(DhcpOption::VendorExtensions(PXE_VENDOR_OPTIONS.to_vec()));
        if let Some(uid) = req.opts().get(OptionCode::ClientMachineIdentifier).cloned() {
            opts.insert(uid);
        }
        reply
    } else {
        if matches!(msg_type, MessageType::Release | MessageType::Decline) {
            state.release_dhcp_lease(mac);
            return None;
        }

        let assigned_ip = if let Some(unit) = static_unit {
            unit.static_ip()
        } else if let DhcpMode::Static(low, high) = iface.dhcp
            && is_netboot_request(&req)
        {
            state.allocate_dhcp_ip(mac, low, high)?
        } else {
            return None;
        };

        let server_ip = iface.network.addr();
        let reply_type = match msg_type {
            MessageType::Discover => MessageType::Offer,
            MessageType::Request => {
                if let Some(DhcpOption::ServerIdentifier(sid)) =
                    req.opts().get(OptionCode::ServerIdentifier)
                    && *sid != server_ip
                {
                    return None;
                }
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
                        nak.to_vec().ok()?,
                        SocketAddrV4::new(Ipv4Addr::BROADCAST, CLIENT_PORT),
                    ));
                }
                MessageType::Ack
            }
            _ => return None,
        };

        state.record_dhcp_lease(assigned_ip, mac);

        let mut reply = build_base_reply(&req, assigned_ip, server_ip, reply_type);
        let opts = reply.opts_mut();
        opts.insert(DhcpOption::AddressLeaseTime(DEFAULT_LEASE_SECS));
        opts.insert(DhcpOption::SubnetMask(iface.network.netmask()));
        opts.insert(DhcpOption::BroadcastAddr(iface.network.broadcast()));
        opts.insert(DhcpOption::Router(vec![server_ip]));
        opts.insert(DhcpOption::DomainNameServer(vec![server_ip]));
        if let Some(hostname) = state.hostmap().get(&assigned_ip) {
            opts.insert(DhcpOption::Hostname(hostname.clone()));
        }
        reply
    };

    let dst = if is_pxe_service_port
        && let SocketAddr::V4(v4) = peer_addr
        && !v4.ip().is_unspecified()
    {
        v4
    } else if !req.giaddr().is_unspecified() {
        SocketAddrV4::new(req.giaddr(), SERVER_PORT)
    } else if !req.ciaddr().is_unspecified() && !req.flags().broadcast() {
        SocketAddrV4::new(req.ciaddr(), CLIENT_PORT)
    } else {
        SocketAddrV4::new(Ipv4Addr::BROADCAST, CLIENT_PORT)
    };

    Some((reply.to_vec().ok()?, dst))
}

async fn serve_socket(
    state: Arc<State>,
    iface: InterfaceConfig,
    socket: UdpSocket,
    is_pxe_service_port: bool,
) -> Result<()> {
    let mut buf = [0u8; 2048];
    loop {
        let (len, peer_addr) = tokio::select! {
            res = socket.recv_from(&mut buf) => res?,
            _ = state.cancel_token.cancelled() => break,
        };

        if let Some((reply_bytes, dst)) =
            handle_dhcp_packet(&state, &iface, &buf[..len], peer_addr, is_pxe_service_port)
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
            dhcp_sock,
            false,
        )));

        if let DhcpMode::Proxy(proxy_ip) = iface.dhcp {
            let (proxy_iface, _) = find_network(proxy_ip)?;
            ensure!(
                proxy_iface == iface_name,
                "Proxy IP {proxy_ip} is on interface {proxy_iface}, expected {iface_name}"
            );
            let pxe_sock = bind_interface_udp(PXE_PORT, &iface_name)?;
            log::info!("ProxyDHCP PXE service listening on {iface_name}:{PXE_PORT} ({proxy_ip})");
            tasks.push(tokio::spawn(serve_socket(
                state.clone(),
                iface.clone(),
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
