use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::sync::atomic::{AtomicU64, Ordering};
use core::task::Waker;
use core::time::Duration;

use dhcparse::dhcpv4::{
    CLIENT_PORT, DhcpOption, Encode as _, Encoder, Flags, Message, MessageType, OpCode, SERVER_PORT,
};
use futures::future::{Either, select};
use spin::Mutex;
use uefi::Handle;
use uefi::proto::Protocol;
use uefi::proto::console::text::Color;
use uefi::proto::device_path::DevicePath;
use uefi::proto::device_path::build::DevicePathBuilder;
use uefi::proto::device_path::text::{AllowShortcuts, DevicePathToText, DisplayOnly};
use uefi::proto::network::snp::SimpleNetwork;

use crate::os::boot_options::BootOptions;
use crate::os::executor::Executor;
use crate::os::executor::event::{Event as ExecutorEvent, EventTrigger};
use crate::os::net::interface::SnpDevice;
pub use crate::os::net::udp::UdpSocket;
use crate::os::send_wrapper::SendWrapper;
use crate::os::timer::rdtsc;
use crate::os::ui;

mod interface;
mod speed;
mod udp;
mod wire;

pub const ETH_PACKET_SIZE: usize = 1514;

static EPHEMERAL_PORT_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(super) struct UdpSocketState {
    pub rx_queue: VecDeque<(Vec<u8>, SocketAddrV4)>,
    pub rx_waker: Option<Waker>,
}

struct NetworkData {
    device: SnpDevice,
    mac: [u8; 6],
    ip: Option<Ipv4Addr>,
    subnet_mask: Ipv4Addr,
    router: Option<Ipv4Addr>,
    arp_table: Vec<(Ipv4Addr, [u8; 6])>,
    arp_wakers: Vec<Waker>,
    udp_sockets: Vec<(u16, UdpSocketState)>,
}

impl NetworkData {
    fn in_same_subnet(&self, addr: Ipv4Addr) -> bool {
        let Some(our_ip) = self.ip else {
            return false;
        };
        let mask = u32::from(self.subnet_mask);
        (u32::from(our_ip) & mask) == (u32::from(addr) & mask)
    }

    fn is_subnet_broadcast(&self, addr: Ipv4Addr) -> bool {
        let Some(our_ip) = self.ip else {
            return false;
        };
        let mask = u32::from(self.subnet_mask);
        mask != u32::MAX
            && (u32::from(addr) & mask) == (u32::from(our_ip) & mask)
            && (u32::from(addr) & !mask) == !mask
    }

    fn next_hop(&self, dst_ip: Ipv4Addr) -> Ipv4Addr {
        if self.in_same_subnet(dst_ip) {
            dst_ip
        } else {
            self.router.unwrap_or(dst_ip)
        }
    }

    fn lookup_arp(&self, ip: Ipv4Addr) -> Option<[u8; 6]> {
        self.arp_table
            .iter()
            .find_map(|&(k, v)| (k == ip).then_some(v))
    }

    fn learn_arp(&mut self, ip: Ipv4Addr, mac: [u8; 6]) {
        if ip.is_unspecified() || ip.is_broadcast() || mac == [0xff; 6] {
            return;
        }
        if let Some(entry) = self.arp_table.iter_mut().find(|(k, _)| *k == ip) {
            entry.1 = mac;
        } else {
            if self.arp_table.len() >= 64 {
                self.arp_table.remove(0);
            }
            self.arp_table.push((ip, mac));
        }
        for waker in self.arp_wakers.drain(..) {
            waker.wake();
        }
    }

    fn send_arp(&mut self, op: u16, target_mac: [u8; 6], target_ip: Ipv4Addr) {
        let sender_ip = self.ip.unwrap_or(Ipv4Addr::UNSPECIFIED);
        let mac = self.mac;
        self.device.transmit_with(|buf| {
            wire::write_arp_eth_ipv4(buf, op, mac, sender_ip, target_mac, target_ip)
        });
    }
}

static NETWORK_DATA: Mutex<Option<NetworkData>> = Mutex::new(None);

static WAITING_FOR_IP: Mutex<Vec<EventTrigger>> = Mutex::new(vec![]);

fn with_net<T, F: FnOnce(&mut NetworkData) -> T>(f: F) -> T {
    let mut mg = NETWORK_DATA.try_lock().expect("Network is locked");
    f(mg.as_mut().expect("Network is not initialized"))
}

fn device_path_to_string(device: &DevicePath) -> String {
    let handle = uefi::boot::get_handle_for_protocol::<DevicePathToText>().unwrap();
    let device_path_to_text =
        uefi::boot::open_protocol_exclusive::<DevicePathToText>(handle).unwrap();
    device_path_to_text
        .convert_device_path_to_text(device, DisplayOnly(true), AllowShortcuts(true))
        .unwrap()
        .to_string()
}

/// Find the topmost device that implements this protocol.
fn handle_on_device<P: Protocol>(device: &DevicePath) -> Option<Handle> {
    for i in 0..device.node_iter().count() {
        let mut buf = vec![];
        let mut dev = DevicePathBuilder::with_vec(&mut buf);
        for node in device.node_iter().take(i + 1) {
            dev = dev.push(&node).unwrap();
        }
        let mut dev = dev.finalize().unwrap();
        if let Ok(h) = uefi::boot::locate_device_path::<P>(&mut dev) {
            return Some(h);
        }
    }
    None
}

pub(super) fn init() {
    let curopt = BootOptions::get(BootOptions::current());
    let (descr, device) = BootOptions::boot_entry_info(&curopt[..]);
    log::info!(
        "Configuring network on interface used for booting ({} -- {})",
        descr,
        device_path_to_string(device)
    );

    let snp_handle = if let Some(handle) = handle_on_device::<SimpleNetwork>(device) {
        handle
    } else {
        log::info!("SNP handle not found on device, falling back to first SNP handle");
        uefi::boot::find_handles::<SimpleNetwork>().unwrap()[0]
    };

    let snp = uefi::boot::open_protocol_exclusive::<SimpleNetwork>(snp_handle).unwrap();
    let mac: [u8; 6] = snp.mode().current_address.0[..6].try_into().unwrap();
    let device = SnpDevice::new(SendWrapper(snp));

    *NETWORK_DATA.lock() = Some(NetworkData {
        device,
        mac,
        ip: None,
        subnet_mask: Ipv4Addr::new(255, 255, 255, 0),
        router: None,
        arp_table: Vec::new(),
        arp_wakers: Vec::new(),
        udp_sockets: Vec::new(),
    });

    Executor::spawn("[net_poll]", async {
        loop {
            if poll() {
                Executor::sched_yield().await;
            } else {
                Executor::wait_for_interrupt().await;
            }
        }
    });

    Executor::spawn("[dhcp]", dhcp_task());

    Executor::spawn("[show_ip]", async {
        let mut draw_area = ui::DrawArea::ip();
        loop {
            draw_area.clear();
            let ip = ip();
            let w = draw_area.size().0;
            if let Some(ip) = ip {
                write!(draw_area, "IP: {ip:>0$}", w - 4).unwrap();
                Executor::sleep(Duration::from_secs(10)).await
            } else {
                draw_area.write_with_color("DHCP...", Color::Yellow, Color::Black);
                Executor::sleep(Duration::from_millis(100)).await
            }
        }
    });

    speed::spawn_network_speed_task();
}

async fn dhcp_task() {
    let sock = UdpSocket::bind_raw(CLIENT_PORT);
    let mac = with_net(|net| net.mac);
    let mut xid = (rdtsc() as u32) ^ u32::from_be_bytes([mac[2], mac[3], mac[4], mac[5]]);
    if xid == 0 {
        xid = 0x1234_5678;
    }

    let mut tx_buf = [0u8; 576];
    let mut rx_buf = [0u8; ETH_PACKET_SIZE];
    let broadcast_server = SocketAddrV4::new(Ipv4Addr::BROADCAST, SERVER_PORT);

    loop {
        xid = xid.wrapping_add(1);
        let mut base = Message::default();
        base.set_op(OpCode::BootRequest);
        base.as_mut()[1] = 1; // Ethernet hardware type
        base.set_xid(xid);
        base.set_flags(Flags::BROADCAST);
        base.set_chaddr(&mac).unwrap();

        let discover_len = Encoder
            .append_options([
                DhcpOption::MessageType(MessageType::DISCOVER),
                DhcpOption::VendorClassIdentifier(b"pixie"),
                DhcpOption::ParameterRequestList(&[1, 3, 54]),
            ])
            .encode(&base, &mut tx_buf)
            .unwrap()
            .as_ref()
            .len();
        let _ = sock
            .send_to(broadcast_server, &tx_buf[..discover_len])
            .await;

        let mut offer = None;
        {
            let mut timeout = Box::pin(Executor::sleep(Duration::from_secs(2)));
            loop {
                let recv = Box::pin(sock.recv_from(&mut rx_buf));
                match select(recv, timeout).await {
                    Either::Left(((data, _), next_timeout)) => {
                        timeout = next_timeout;
                        if let Ok(msg) = Message::new(data)
                            && msg.op() == Ok(OpCode::BootReply)
                            && msg.xid() == xid
                            && msg.chaddr() == Ok(&mac[..])
                            && msg.yiaddr().0 != [0, 0, 0, 0]
                            && let Ok((MessageType::OFFER, Some(server_id))) =
                                dhcparse::v4_options!(msg; MessageType required, ServerIdentifier)
                        {
                            offer = Some((*msg.yiaddr(), *server_id));
                            break;
                        }
                    }
                    Either::Right(((), _)) => break,
                }
            }
        }

        let Some((offered_ip, server_id)) = offer else {
            continue;
        };

        let request_len = Encoder
            .append_options([
                DhcpOption::MessageType(MessageType::REQUEST),
                DhcpOption::RequestedIpAddress(&offered_ip),
                DhcpOption::ServerIdentifier(&server_id),
                DhcpOption::VendorClassIdentifier(b"pixie"),
                DhcpOption::ParameterRequestList(&[1, 3, 54]),
            ])
            .encode(&base, &mut tx_buf)
            .unwrap()
            .as_ref()
            .len();
        let _ = sock.send_to(broadcast_server, &tx_buf[..request_len]).await;

        let mut timeout = Box::pin(Executor::sleep(Duration::from_secs(2)));
        let configured = loop {
            let recv = Box::pin(sock.recv_from(&mut rx_buf));
            match select(recv, timeout).await {
                Either::Left(((data, _), next_timeout)) => {
                    timeout = next_timeout;
                    if let Ok(msg) = Message::new(data)
                        && msg.op() == Ok(OpCode::BootReply)
                        && msg.xid() == xid
                        && msg.chaddr() == Ok(&mac[..])
                        && msg.yiaddr().0 != [0, 0, 0, 0]
                        && let Ok((MessageType::ACK, subnet_mask, router)) =
                            dhcparse::v4_options!(msg; MessageType required, SubnetMask, Router)
                    {
                        let yiaddr = Ipv4Addr::from(msg.yiaddr().0);
                        let mask = subnet_mask
                            .map(|a| Ipv4Addr::from(a.0))
                            .unwrap_or(Ipv4Addr::new(255, 255, 255, 0));
                        let gw = router.and_then(|r| r.first()).map(|a| Ipv4Addr::from(a.0));
                        with_net(|net| {
                            net.ip = Some(yiaddr);
                            net.subnet_mask = mask;
                            net.router = gw;
                        });
                        let to_wake = core::mem::take(&mut *WAITING_FOR_IP.lock());
                        for e in to_wake {
                            e.trigger();
                        }
                        break true;
                    }
                }
                Either::Right(((), _)) => break false,
            }
        };

        if configured {
            break;
        }
    }
}

pub async fn wait_for_ip() {
    if ip().is_some() {
        return;
    }
    let event = ExecutorEvent::new();
    WAITING_FOR_IP.lock().push(event.trigger());
    event.await;
}

fn ip() -> Option<Ipv4Addr> {
    with_net(|n| n.ip)
}

fn get_ephemeral_port() -> u16 {
    let ans = EPHEMERAL_PORT_COUNTER.fetch_add(1, Ordering::Relaxed);
    ((ans % (60999 - 49152)) + 49152) as u16
}

/// Drains up to 64 frames from the NIC and returns true if the limit was reached
/// (meaning more frames may be immediately available).
pub(super) fn poll() -> bool {
    let mut rx_buf = [0u8; ETH_PACKET_SIZE + 4];
    let mut mg = NETWORK_DATA.lock();
    let net = mg.as_mut().unwrap();

    for _ in 0..64 {
        let Some(frame) = net.device.receive(&mut rx_buf) else {
            return false;
        };
        let Some((eth_src, ether_type, eth_payload)) = wire::parse_ethernet(frame) else {
            continue;
        };

        match ether_type {
            wire::ETHERTYPE_ARP => {
                let Some(arp) = wire::parse_arp_eth_ipv4(eth_payload) else {
                    continue;
                };
                net.learn_arp(arp.sender_ip, arp.sender_mac);

                if arp.op == wire::ARP_OP_REQUEST && net.ip == Some(arp.target_ip) {
                    net.send_arp(wire::ARP_OP_REPLY, arp.sender_mac, arp.sender_ip);
                }
            }
            wire::ETHERTYPE_IPV4 => {
                let Some(ipv4) = wire::parse_ipv4(eth_payload) else {
                    continue;
                };
                let src_ip = ipv4.src_ip;
                let dst_ip = ipv4.dst_ip;
                if net.ip.is_none() || net.in_same_subnet(src_ip) {
                    net.learn_arp(src_ip, eth_src);
                }
                let for_us = dst_ip == Ipv4Addr::BROADCAST
                    || net.ip.is_none()
                    || net.ip == Some(dst_ip)
                    || net.is_subnet_broadcast(dst_ip);
                if !for_us || ipv4.protocol != wire::IP_PROTO_UDP {
                    continue;
                }
                let Some((sport, dport, payload)) = wire::parse_udp(ipv4.payload, src_ip, dst_ip)
                else {
                    continue;
                };
                if let Some((_, sock)) = net.udp_sockets.iter_mut().find(|(p, _)| *p == dport) {
                    if sock.rx_queue.len() < 1024 {
                        sock.rx_queue
                            .push_back((payload.to_vec(), SocketAddrV4::new(src_ip, sport)));
                    }
                    if let Some(waker) = sock.rx_waker.take() {
                        waker.wake();
                    }
                }
            }
            _ => {}
        }
    }
    true
}
