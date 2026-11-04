use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::fmt::Write;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::sync::atomic::{AtomicU64, Ordering};
use core::task::Waker;
use core::time::Duration;

use dhcparse::dhcpv4::{
    CLIENT_PORT, DhcpOption, Encode as _, Encoder, Flags, Message, MessageType, OpCode, SERVER_PORT,
};
use etherparse::{
    ArpEthIpv4Packet, ArpHardwareId, ArpOperation, ArpPacketSlice, EtherType, Ethernet2Header,
    Ethernet2Slice, IpNumber, Ipv4HeaderSlice, UdpSlice,
};
use futures::future::{Either, select};
use spin::Mutex;
use uefi::proto::console::text::Color;

use crate::executor::Executor;
use crate::executor::event::{Event, EventTrigger};
use crate::net::interface::KernelNicDevice;
pub use crate::net::udp::UdpSocket;
use crate::timer::rdtsc;
use crate::ui;

pub mod interface;
pub mod speed;
pub mod udp;

pub const ETH_PACKET_SIZE: usize = 1514;
const MAX_ARP_ENTRIES: usize = 64;

static EPHEMERAL_PORT_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) struct UdpSocketState {
    pub rx_queue: VecDeque<(Vec<u8>, SocketAddrV4)>,
    pub rx_waker: Option<Waker>,
}

pub(crate) struct NetworkData {
    pub device: KernelNicDevice,
    pub mac: [u8; 6],
    pub ip: Option<Ipv4Addr>,
    pub subnet_mask: Ipv4Addr,
    pub router: Option<Ipv4Addr>,
    pub arp_table: Vec<(Ipv4Addr, [u8; 6])>,
    pub arp_wakers: Vec<Waker>,
    pub udp_sockets: Vec<(u16, UdpSocketState)>,
}

impl NetworkData {
    pub(crate) fn in_same_subnet(&self, addr: Ipv4Addr) -> bool {
        let Some(our_ip) = self.ip else {
            return false;
        };
        let mask = u32::from(self.subnet_mask);
        (u32::from(our_ip) & mask) == (u32::from(addr) & mask)
    }

    pub(crate) fn is_subnet_broadcast(&self, addr: Ipv4Addr) -> bool {
        let Some(our_ip) = self.ip else {
            return false;
        };
        let mask = u32::from(self.subnet_mask);
        mask != u32::MAX
            && (u32::from(addr) & mask) == (u32::from(our_ip) & mask)
            && (u32::from(addr) & !mask) == !mask
    }

    pub(crate) fn next_hop(&self, dst_ip: Ipv4Addr) -> Ipv4Addr {
        if self.in_same_subnet(dst_ip) {
            dst_ip
        } else {
            self.router.unwrap_or(dst_ip)
        }
    }

    pub(crate) fn lookup_arp(&self, ip: Ipv4Addr) -> Option<[u8; 6]> {
        self.arp_table
            .iter()
            .find_map(|&(k, v)| (k == ip).then_some(v))
    }

    pub(crate) fn learn_arp(&mut self, ip: Ipv4Addr, mac: [u8; 6]) {
        if ip.is_unspecified() || ip.is_broadcast() || mac == [0xff; 6] {
            return;
        }
        if let Some(entry) = self.arp_table.iter_mut().find(|(k, _)| *k == ip) {
            entry.1 = mac;
        } else {
            if self.arp_table.len() >= MAX_ARP_ENTRIES {
                self.arp_table.remove(0);
            }
            self.arp_table.push((ip, mac));
        }
        for waker in self.arp_wakers.drain(..) {
            waker.wake();
        }
    }

    pub(crate) fn send_arp(&mut self, op: ArpOperation, target_mac: [u8; 6], target_ip: Ipv4Addr) {
        let eth_dst = if op == ArpOperation::REQUEST {
            [0xff; 6]
        } else {
            target_mac
        };
        let sender_ip = self.ip.unwrap_or(Ipv4Addr::UNSPECIFIED).octets();
        let eth = Ethernet2Header {
            source: self.mac,
            destination: eth_dst,
            ether_type: EtherType::ARP,
        }
        .to_bytes();
        let arp = ArpEthIpv4Packet {
            operation: op,
            sender_mac: self.mac,
            sender_ipv4: sender_ip,
            target_mac,
            target_ipv4: target_ip.octets(),
        }
        .to_bytes();
        self.device.transmit_with(|buf| {
            buf[..eth.len()].copy_from_slice(&eth);
            buf[eth.len()..eth.len() + arp.len()].copy_from_slice(&arp);
            eth.len() + arp.len()
        });
    }
}

pub(crate) static NETWORK_DATA: Mutex<Option<NetworkData>> = Mutex::new(None);
pub(crate) static WAITING_FOR_IP: Mutex<Vec<EventTrigger>> = Mutex::new(Vec::new());

pub(crate) fn with_net<T, F: FnOnce(&mut NetworkData) -> T>(f: F) -> T {
    let mut mg = NETWORK_DATA.try_lock().expect("Network is locked");
    f(mg.as_mut().expect("Network is not initialized"))
}

pub fn init() {
    use crate::hal::NativeHal;
    use crate::net::interface::KernelNic;
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

    if matched_nic.is_none() {
        for &pci in &pci_devs {
            // xHCI USB 3.0 controller: class 0x0C, subclass 0x03, prog_if 0x30
            if pci.class_code == 0x0C && pci.subclass == 0x03 && pci.prog_if == 0x30 {
                pci.enable_bus_mastering(&NativeHal);
                if let Some(Bar::Memory {
                    base: bar0, size, ..
                }) = pci.read_bar(&NativeHal, 0)
                {
                    crate::arch::paging::map_mmio(bar0, size.max(0x40000));
                    if let Ok(dev) = UsbEthernetDevice::new(bar0, fallback_mac, NativeHal)
                        && (first_pci_nic.is_none() || Some(dev.mac_address()) == fallback_mac)
                    {
                        matched_nic = Some(KernelNic::Usb(dev));
                        break;
                    }
                }
            }
        }
    }

    let nic = matched_nic.or(first_pci_nic).expect(
        "No supported network interface found (Intel e1000/e1000e/igc, Realtek RTL8168/8125, or USB Ethernet)",
    );

    match &nic {
        KernelNic::E1000(dev) => {
            log::info!(
                "Initialized Intel e1000/e1000e NIC (MAC {:02x?})",
                dev.mac_address()
            );
        }
        KernelNic::Igc(dev) => {
            log::info!(
                "Initialized Intel igc I225/I226 NIC (MAC {:02x?})",
                dev.mac_address()
            );
        }
        KernelNic::Rtl(dev) => {
            log::info!("Initialized Realtek NIC (MAC {:02x?})", dev.mac_address());
        }
        KernelNic::Usb(dev) => {
            log::info!(
                "Initialized USB Ethernet dongle ({:?}, MAC {:02x?})",
                dev.protocol(),
                dev.mac_address()
            );
        }
    }

    let mac = nic.mac_address();
    let device = KernelNicDevice {
        nic,
        tx_buf: [0u8; ETH_PACKET_SIZE],
    };

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
            let cur_ip = ip();
            let w = draw_area.size().0;
            if let Some(ip) = cur_ip {
                let _ = write!(draw_area, "IP: {ip:>0$}", w.saturating_sub(4));
                Executor::sleep(Duration::from_secs(10)).await;
            } else {
                draw_area.write_with_color("DHCP...", Color::Yellow, Color::Black);
                Executor::sleep(Duration::from_millis(100)).await;
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
                        log::info!("DHCP configured: IP {yiaddr}");
                        with_net(|net| {
                            net.ip = Some(yiaddr);
                            net.subnet_mask = mask;
                            net.router = gw;
                        });
                        for e in WAITING_FOR_IP.lock().drain(..) {
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
    while ip().is_none() {
        let event = Event::new();
        WAITING_FOR_IP.lock().push(event.trigger());
        if ip().is_some() {
            break;
        }
        event.await;
    }
}

pub fn usb_nic_mmio_base() -> Option<u64> {
    let mg = NETWORK_DATA.lock();
    match &mg.as_ref()?.device.nic {
        crate::net::interface::KernelNic::Usb(dev) => Some(dev.mmio_base),
        _ => None,
    }
}

pub fn ip() -> Option<Ipv4Addr> {
    with_net(|net| net.ip)
}

pub(crate) fn get_ephemeral_port() -> u16 {
    const START: u64 = 49152;
    const RANGE: u64 = 61000 - START;
    with_net(|net| {
        for _ in 0..RANGE {
            let ans = EPHEMERAL_PORT_COUNTER.fetch_add(1, Ordering::Relaxed);
            let port = ((ans % RANGE) + START) as u16;
            if !net.udp_sockets.iter().any(|(p, _)| *p == port) {
                return port;
            }
        }
        49152
    })
}

fn ones_complement_add(mut sum: u32, bytes: &[u8]) -> u32 {
    let (chunks, rem) = bytes.as_chunks::<2>();
    for &chunk in chunks {
        sum += u16::from_be_bytes(chunk) as u32;
    }
    if let &[last] = rem {
        sum += ((last as u16) << 8) as u32;
    }
    sum
}

fn fold_ones_complement(mut sum: u32) -> u16 {
    while (sum >> 16) != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    sum as u16
}

fn verify_ipv4_checksum(hdr_slice: &[u8]) -> bool {
    fold_ones_complement(ones_complement_add(0, hdr_slice)) == 0xFFFF
}

fn verify_udp_checksum(src_ip: Ipv4Addr, dst_ip: Ipv4Addr, udp_slice: &[u8]) -> bool {
    let mut sum = 0u32;
    sum = ones_complement_add(sum, &src_ip.octets());
    sum = ones_complement_add(sum, &dst_ip.octets());
    sum += IpNumber::UDP.0 as u32;
    sum += udp_slice.len() as u32;
    sum = ones_complement_add(sum, udp_slice);
    fold_ones_complement(sum) == 0xFFFF
}

fn poll() -> bool {
    let mut rx_buf = [0u8; ETH_PACKET_SIZE + 4];
    let mut mg = NETWORK_DATA.lock();
    let net = mg.as_mut().unwrap();

    for _ in 0..64 {
        let Some(frame) = net.device.receive(&mut rx_buf) else {
            return net.device.nic.has_packets();
        };
        let Ok(eth) = Ethernet2Slice::from_slice_without_fcs(frame) else {
            continue;
        };
        let eth_src = eth.source();
        let eth_payload = eth.payload().payload;

        match eth.ether_type() {
            EtherType::ARP => {
                let Ok(arp) = ArpPacketSlice::from_slice(eth_payload) else {
                    continue;
                };
                if arp.hw_addr_type() != ArpHardwareId::ETHERNET
                    || arp.proto_addr_type() != EtherType::IPV4
                    || arp.hw_addr_size() != 6
                    || arp.proto_addr_size() != 4
                {
                    continue;
                }
                let sender_mac: [u8; 6] = arp.sender_hw_addr().try_into().unwrap();
                let sender_ip =
                    Ipv4Addr::from(<[u8; 4]>::try_from(arp.sender_protocol_addr()).unwrap());
                net.learn_arp(sender_ip, sender_mac);

                if arp.operation() == ArpOperation::REQUEST {
                    let target_ip =
                        Ipv4Addr::from(<[u8; 4]>::try_from(arp.target_protocol_addr()).unwrap());
                    if net.ip == Some(target_ip) {
                        net.send_arp(ArpOperation::REPLY, sender_mac, sender_ip);
                    }
                }
            }
            EtherType::IPV4 => {
                let Ok(ipv4) = Ipv4HeaderSlice::from_slice(eth_payload) else {
                    continue;
                };
                let total_len = ipv4.total_len() as usize;
                let hdr_len = ipv4.slice().len();
                if total_len < hdr_len
                    || eth_payload.len() < total_len
                    || ipv4.is_fragmenting_payload()
                    || !verify_ipv4_checksum(ipv4.slice())
                {
                    continue;
                }
                let src_ip = ipv4.source_addr();
                let dst_ip = ipv4.destination_addr();
                if net.ip.is_none() || net.in_same_subnet(src_ip) {
                    net.learn_arp(src_ip, eth_src);
                }
                let for_us = dst_ip == Ipv4Addr::BROADCAST
                    || net.ip.is_none()
                    || net.ip == Some(dst_ip)
                    || net.is_subnet_broadcast(dst_ip);
                if !for_us || ipv4.protocol() != IpNumber::UDP {
                    continue;
                }
                let ip_payload = &eth_payload[hdr_len..total_len];
                let Ok(udp) = UdpSlice::from_slice(ip_payload) else {
                    continue;
                };
                let udp_len = udp.length() as usize;
                if udp.checksum() != 0
                    && !verify_udp_checksum(src_ip, dst_ip, &ip_payload[..udp_len])
                {
                    continue;
                }
                let dport = udp.destination_port();
                if let Some((_, sock)) = net.udp_sockets.iter_mut().find(|(p, _)| *p == dport) {
                    if sock.rx_queue.len() < 1024 {
                        sock.rx_queue.push_back((
                            udp.payload().to_vec(),
                            SocketAddrV4::new(src_ip, udp.source_port()),
                        ));
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
