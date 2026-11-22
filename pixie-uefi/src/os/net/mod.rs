use alloc::collections::VecDeque;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::sync::atomic::{AtomicU64, Ordering};
use core::task::Waker;
use core::time::Duration;

use pixie_shared::client_net::packet::{ARP_OP_REPLY, ARP_OP_REQUEST};
use pixie_shared::client_net::{
    ArpCache, BROADCAST_MAC, DhcpClient, DhcpEvent, RxFrame, parse_rx_frame, write_arp_frame,
    write_icmp_echo_reply, write_udp_frame,
};
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
use crate::os::net::snp::{RxPool, SnpNic};
pub use crate::os::net::udp::UdpSocket;
use crate::os::send_wrapper::SendWrapper;
use crate::os::timer::{Timer, rdtsc};
use crate::os::ui;

mod snp;
mod speed;
mod udp;

pub use pixie_shared::client_net::ETH_PACKET_SIZE;

const MAX_RX_BATCH: usize = 1024;
const SOCKET_QUEUE_CAP: usize = 1000;

static EPHEMERAL_PORT_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(super) struct RxPacket {
    pub(super) buf_idx: u16,
    pub(super) offset: usize,
    pub(super) len: usize,
    pub(super) src: SocketAddrV4,
}

pub(super) struct UdpSocketSlot {
    pub(super) port: u16,
    pub(super) queue: VecDeque<RxPacket>,
    pub(super) recv_waker: Option<Waker>,
}

pub(super) struct NetStack {
    nic: SnpNic,
    pub(super) rx_pool: RxPool,
    arp: ArpCache,
    dhcp: DhcpClient,
    ip_id: u16,
    pub(super) sockets: Vec<Option<UdpSocketSlot>>,
}

impl NetStack {
    fn next_ip_id(&mut self) -> u16 {
        let id = self.ip_id;
        self.ip_id = self.ip_id.wrapping_add(1);
        id
    }

    fn send_arp(
        &mut self,
        dst_mac: [u8; 6],
        op: u16,
        sender_ip: Ipv4Addr,
        target_mac: [u8; 6],
        target_ip: Ipv4Addr,
    ) {
        let my_mac = self.nic.mac();
        self.nic.transmit(|buf| {
            write_arp_frame(
                buf, my_mac, dst_mac, op, my_mac, sender_ip, target_mac, target_ip,
            )
        });
    }

    fn send_udp_frame(
        &mut self,
        dst_mac: [u8; 6],
        src_ip: Ipv4Addr,
        dst_ip: Ipv4Addr,
        src_port: u16,
        dst_port: u16,
        payload: &[u8],
    ) {
        let my_mac = self.nic.mac();
        let ip_id = self.next_ip_id();
        self.nic.transmit(|buf| {
            write_udp_frame(
                buf, my_mac, dst_mac, src_ip, dst_ip, src_port, dst_port, ip_id, payload,
            )
        });
    }

    pub(super) fn send_udp(
        &mut self,
        dst_ip: Ipv4Addr,
        src_port: u16,
        dst_port: u16,
        payload: &[u8],
    ) {
        let Some(cfg) = self.dhcp.config().copied() else {
            return;
        };
        let now_us = Timer::micros();
        let (next_hop, dst_mac) = if dst_ip.is_broadcast() || dst_ip == cfg.broadcast_ip() {
            (dst_ip, Some(BROADCAST_MAC))
        } else {
            let nh = ArpCache::next_hop(cfg.ip, cfg.netmask, cfg.router, dst_ip);
            (nh, self.arp.lookup(nh, now_us))
        };

        if let Some(dst_mac) = dst_mac {
            self.send_udp_frame(dst_mac, cfg.ip, dst_ip, src_port, dst_port, payload);
        } else if self.arp.should_request(next_hop, now_us) {
            self.send_arp(BROADCAST_MAC, ARP_OP_REQUEST, cfg.ip, [0; 6], next_hop);
        }
    }

    pub(super) fn alloc_ephemeral_port(&self) -> u16 {
        const RANGE: u64 = 60999 - 49152;
        for _ in 0..RANGE {
            let ans = EPHEMERAL_PORT_COUNTER.fetch_add(1, Ordering::Relaxed);
            let port = ((ans % RANGE) + 49152) as u16;
            if !self.sockets.iter().flatten().any(|s| s.port == port) {
                return port;
            }
        }
        let ans = EPHEMERAL_PORT_COUNTER.fetch_add(1, Ordering::Relaxed);
        ((ans % RANGE) + 49152) as u16
    }
}

static NETWORK_DATA: Mutex<Option<NetStack>> = Mutex::new(None);
static WAITING_FOR_IP: Mutex<Vec<EventTrigger>> = Mutex::new(vec![]);

pub(super) fn with_net<T, F: FnOnce(&mut NetStack) -> T>(f: F) -> T {
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
    let nic = SnpNic::new(SendWrapper(snp));
    let xid = (rdtsc() as u32) ^ u32::from_le_bytes(nic.mac()[2..6].try_into().unwrap());
    let dhcp = DhcpClient::new(nic.mac(), xid, Timer::micros());

    *NETWORK_DATA.lock() = Some(NetStack {
        nic,
        rx_pool: RxPool::new(),
        arp: ArpCache::default(),
        dhcp,
        ip_id: 1,
        sockets: Vec::new(),
    });

    Executor::spawn("[net_poll]", async {
        loop {
            if poll() {
                Executor::sched_yield().await;
            } else {
                futures::future::select(
                    Executor::wait_for_interrupt(),
                    Executor::sleep(Duration::from_millis(10)),
                )
                .await;
            }
        }
    });

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

pub async fn wait_for_ip() {
    if ip().is_some() {
        return;
    }
    let event = ExecutorEvent::new();
    WAITING_FOR_IP.lock().push(event.trigger());
    event.await;
}

fn ip() -> Option<Ipv4Addr> {
    with_net(|n| n.dhcp.config().map(|c| c.ip))
}

/// Polls up to `MAX_RX_BATCH` incoming Ethernet frames and drives DHCP/ARP timers.
/// Returns `true` if at least one frame was received from the NIC.
fn poll() -> bool {
    let now_us = Timer::micros();
    let mut processed = 0usize;
    let mut wake_ip_waiters = false;

    {
        let mut guard = NETWORK_DATA.lock();
        let net = guard.as_mut().unwrap();
        net.nic.drain_recycled();

        while processed < MAX_RX_BATCH {
            let Some(buf_idx) = net.rx_pool.alloc() else {
                break;
            };
            let Some(len) = net.nic.try_recv(net.rx_pool.buf_mut(buf_idx)) else {
                net.rx_pool.free(buf_idx);
                break;
            };
            processed += 1;

            let cfg = net.dhcp.config().copied();
            let parsed = parse_rx_frame(
                &net.rx_pool.buf(buf_idx)[..len],
                net.nic.mac(),
                cfg.map(|c| c.ip),
                cfg.map(|c| c.netmask),
            );

            match parsed {
                Some(RxFrame::Arp(arp_pkt)) => {
                    net.rx_pool.free(buf_idx);
                    net.arp.learn(arp_pkt.sender_ip, arp_pkt.sender_mac, now_us);

                    if arp_pkt.op == ARP_OP_REQUEST
                        && let Some(c) = cfg
                        && c.ip == arp_pkt.target_ip
                    {
                        net.send_arp(
                            arp_pkt.sender_mac,
                            ARP_OP_REPLY,
                            c.ip,
                            arp_pkt.sender_mac,
                            arp_pkt.sender_ip,
                        );
                    }
                }
                Some(RxFrame::IcmpEcho {
                    src_mac,
                    src_ip,
                    dst_ip,
                    payload_offset,
                    payload_len,
                }) => {
                    let my_mac = net.nic.mac();
                    let ip_id = net.next_ip_id();
                    let rx_pool = &net.rx_pool;
                    net.nic.transmit(|tx| {
                        write_icmp_echo_reply(
                            tx,
                            my_mac,
                            src_mac,
                            dst_ip,
                            src_ip,
                            ip_id,
                            &rx_pool.buf(buf_idx)[payload_offset..payload_offset + payload_len],
                        )
                    });
                    net.rx_pool.free(buf_idx);
                }
                Some(RxFrame::Udp {
                    src_mac,
                    src_ip,
                    dst_ip,
                    src_port,
                    dst_port,
                    payload_offset,
                    payload_len,
                }) => {
                    if !dst_ip.is_broadcast() && cfg.is_some_and(|c| dst_ip == c.ip) {
                        net.arp.learn(src_ip, src_mac, now_us);
                    }

                    if dst_port == 68 {
                        let ev = net.dhcp.on_rx(
                            &net.rx_pool.buf(buf_idx)[payload_offset..payload_offset + payload_len],
                            now_us,
                        );
                        net.rx_pool.free(buf_idx);
                        match ev {
                            Some(DhcpEvent::Bound(new_cfg)) => {
                                net.send_arp(
                                    BROADCAST_MAC,
                                    ARP_OP_REQUEST,
                                    new_cfg.ip,
                                    [0; 6],
                                    new_cfg.ip,
                                );
                                wake_ip_waiters = true;
                            }
                            Some(DhcpEvent::Deconfigured) => {
                                net.arp.clear();
                            }
                            None => {}
                        }
                    } else if let Some(slot) = net
                        .sockets
                        .iter_mut()
                        .flatten()
                        .find(|s| s.port == dst_port)
                        && slot.queue.len() < SOCKET_QUEUE_CAP
                    {
                        slot.queue.push_back(RxPacket {
                            buf_idx,
                            offset: payload_offset,
                            len: payload_len,
                            src: SocketAddrV4::new(src_ip, src_port),
                        });
                        if let Some(w) = slot.recv_waker.take() {
                            w.wake();
                        }
                    } else {
                        net.rx_pool.free(buf_idx);
                    }
                }
                None => {
                    net.rx_pool.free(buf_idx);
                }
            }
        }

        let mut dhcp_buf = [0u8; 320];
        if let Some((dst_ip, len)) = net.dhcp.poll_tx(now_us, &mut dhcp_buf) {
            let src_ip = net.dhcp.config().map_or(Ipv4Addr::UNSPECIFIED, |c| c.ip);
            let dst_mac = if dst_ip.is_broadcast() {
                Some(BROADCAST_MAC)
            } else {
                net.arp.lookup(dst_ip, now_us)
            };
            if let Some(dst_mac) = dst_mac {
                net.send_udp_frame(dst_mac, src_ip, dst_ip, 68, 67, &dhcp_buf[..len]);
            } else if net.arp.should_request(dst_ip, now_us) {
                net.send_arp(BROADCAST_MAC, ARP_OP_REQUEST, src_ip, [0; 6], dst_ip);
            }
        }
    }

    if wake_ip_waiters {
        let to_wake = core::mem::take(&mut *WAITING_FOR_IP.lock());
        for e in to_wake {
            e.trigger();
        }
    }

    processed > 0
}
