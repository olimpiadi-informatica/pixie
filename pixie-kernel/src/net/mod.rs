use alloc::collections::VecDeque;
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
use uefi::proto::console::text::Color;

use crate::executor::Executor;
use crate::executor::event::{Event as ExecutorEvent, EventTrigger};
use crate::net::interface::{KernelNic, KernelNicDevice, RxPool};
pub use crate::net::udp::UdpSocket;
use crate::timer::{Timer, rdtsc};
use crate::ui;

pub mod interface;
pub mod speed;
pub mod udp;

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
    nic: KernelNicDevice,
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
static WAITING_FOR_IP: Mutex<Vec<EventTrigger>> = Mutex::new(Vec::new());

pub(super) fn with_net<T, F: FnOnce(&mut NetStack) -> T>(f: F) -> T {
    let mut mg = NETWORK_DATA.try_lock().expect("Network is locked");
    f(mg.as_mut().expect("Network is not initialized"))
}

pub fn init() {
    use crate::hal::NativeHal;
    use pixie_drivers::net::e1000::{self, E1000Device};
    use pixie_drivers::net::igc::{self, IgcDevice};
    use pixie_drivers::net::rtl::{self, RtlDevice};
    use pixie_drivers::net::usb_net::UsbEthernetDevice;
    use pixie_drivers::pci::{Bar, scan_pci};

    let fallback_mac = crate::boot_info::BOOT_INFO.lock().and_then(|b| b.uefi_mac);
    let pci_devs = scan_pci(&NativeHal);
    let mut matched_nic: Option<KernelNic> = None;
    let mut first_pci_nic: Option<KernelNic> = None;

    let mut accept_pci_candidate = |cand: KernelNic| -> bool {
        if fallback_mac.is_none() || Some(cand.mac_address()) == fallback_mac {
            matched_nic = Some(cand);
            true
        } else {
            if first_pci_nic.is_none() {
                first_pci_nic = Some(cand);
            }
            false
        }
    };

    for &pci in &pci_devs {
        if e1000::probe(&pci) {
            let mut candidate = None;
            for bar_idx in 0..6 {
                if let Some(Bar::Memory { base, size, .. }) = pci.read_bar(&NativeHal, bar_idx) {
                    crate::arch::paging::map_mmio(base, size.max(0x20000));
                    if let Ok(dev) = E1000Device::new(pci, base, fallback_mac, NativeHal) {
                        candidate = Some(KernelNic::E1000(dev));
                        break;
                    }
                }
            }
            if let Some(cand) = candidate
                && accept_pci_candidate(cand)
            {
                break;
            }
        } else if igc::probe(&pci) {
            let mut candidate = None;
            for bar_idx in 0..6 {
                if let Some(Bar::Memory { base, size, .. }) = pci.read_bar(&NativeHal, bar_idx) {
                    crate::arch::paging::map_mmio(base, size.max(0x20000));
                    if let Ok(dev) = IgcDevice::new(pci, base, fallback_mac, NativeHal) {
                        candidate = Some(KernelNic::Igc(dev));
                        break;
                    }
                }
            }
            if let Some(cand) = candidate
                && accept_pci_candidate(cand)
            {
                break;
            }
        } else if rtl::probe(&pci).is_some() {
            let mut reg_bar = None;
            for bar_idx in 0..6 {
                if let Some(Bar::Memory { base, size, .. }) = pci.read_bar(&NativeHal, bar_idx) {
                    reg_bar = Some((base, size, false));
                    break;
                }
            }
            if reg_bar.is_none() {
                for bar_idx in 0..6 {
                    if let Some(Bar::Io { port }) = pci.read_bar(&NativeHal, bar_idx) {
                        reg_bar = Some((port as u64, 0, true));
                        break;
                    }
                }
            }
            if let Some((reg_base, bar_size, is_io)) = reg_bar {
                if !is_io {
                    crate::arch::paging::map_mmio(reg_base, bar_size.max(0x10000));
                }
                if let Ok(dev) = RtlDevice::new(pci, reg_base, is_io, fallback_mac, NativeHal)
                    && accept_pci_candidate(KernelNic::Rtl(dev))
                {
                    break;
                }
            }
        }
    }

    // Check shared xHCI controllers if no matching PCI NIC was found yet
    if matched_nic.is_none() {
        for ctrl in crate::usb::controllers() {
            if let Ok(dev) = UsbEthernetDevice::attach(ctrl.clone(), fallback_mac) {
                matched_nic = Some(KernelNic::Usb(dev));
                break;
            }
        }
    }

    let nic_hw = matched_nic
        .or(first_pci_nic)
        .expect("No supported network card (Intel e1000/e1000e/I225/I226, Realtek 8111/8125, or USB RTL8153/CDC-ECM/NCM) found");

    let mac = nic_hw.mac_address();
    log::info!(
        "Initialized native NIC MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5]
    );

    let nic = KernelNicDevice {
        nic: nic_hw,
        tx_buf: [0; ETH_PACKET_SIZE],
    };

    let now_us = Timer::micros();
    let xid = ((rdtsc() as u32) ^ u32::from_be_bytes([mac[2], mac[3], mac[4], mac[5]])) | 1;
    let dhcp = DhcpClient::new(mac, xid, now_us);

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
                write!(draw_area, "IP: {ip:>0$}", w.saturating_sub(4)).unwrap();
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

pub fn ip() -> Option<Ipv4Addr> {
    with_net(|n| n.dhcp.config().map(|c| c.ip))
}

pub fn mac_addr() -> [u8; 6] {
    with_net(|n| n.nic.mac())
}

pub fn broadcast_ip() -> Ipv4Addr {
    with_net(|n| {
        n.dhcp
            .config()
            .map_or(Ipv4Addr::BROADCAST, |c| c.broadcast_ip())
    })
}

/// Polls up to `MAX_RX_BATCH` incoming Ethernet frames and drives DHCP/ARP timers.
/// Returns `true` if at least one frame was received from the NIC.
pub fn poll() -> bool {
    let now_us = Timer::micros();
    let mut processed = 0usize;
    let mut wake_ip_waiters = false;

    {
        let mut guard = NETWORK_DATA.lock();
        let net = guard.as_mut().unwrap();
        let _ = net.nic.nic.can_transmit();

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
