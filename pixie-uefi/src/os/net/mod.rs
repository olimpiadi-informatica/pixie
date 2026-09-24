use alloc::vec::Vec;
use core::fmt::Write;
use core::net::Ipv4Addr;
use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;

use smoltcp::iface::{
    Config, Interface, PollIngressSingleResult, PollResult, SocketHandle, SocketSet,
};
use smoltcp::socket::dhcpv4::{Event, Socket as Dhcpv4Socket};
use smoltcp::wire::{DhcpOption, HardwareAddress, IpCidr};
use spin::Mutex;
use uefi::proto::console::text::Color;

use super::timer::Timer;
use crate::os::executor::Executor;
use crate::os::executor::event::{Event as ExecutorEvent, EventTrigger};
use crate::os::net::interface::{KernelNic, KernelNicDevice};
pub use crate::os::net::tcp::TcpStream;
pub use crate::os::net::udp::UdpSocket;
use crate::os::timer::rdtsc;
use crate::os::{pci, raw_fb, ui};

pub mod e1000e;
mod interface;
pub mod rtl8169;
mod speed;
mod tcp;
mod udp;

pub const ETH_PACKET_SIZE: usize = 1514;

static EPHEMERAL_PORT_COUNTER: AtomicU64 = AtomicU64::new(0);

struct NetworkData {
    interface: Interface,
    device: KernelNicDevice,
    socket_set: SocketSet<'static>,
    dhcp_socket_handle: SocketHandle,
}

static NETWORK_DATA: Mutex<Option<NetworkData>> = Mutex::new(None);

static WAITING_FOR_IP: Mutex<Vec<EventTrigger>> = Mutex::new(vec![]);

fn with_net<T, F: FnOnce(&mut NetworkData) -> T>(f: F) -> T {
    let mut mg = NETWORK_DATA.try_lock().expect("Network is locked");
    f(mg.as_mut().expect("Network is not initialized"))
}

pub(super) fn init() {
    log::info!("Probing PCI network controllers (e1000/e1000e, rtl8169)...");
    let pci_devices = pci::scan_pci();
    let mut nics: Vec<KernelNic> = Vec::new();

    let mut w = raw_fb::StackWriter::<128>::new();
    let _ = core::write!(w, "[NET] Scanning {} PCI devices for network adapters...", pci_devices.len());
    raw_fb::print(w.as_str(), raw_fb::COLOR_CYAN, raw_fb::COLOR_DARK_BLUE);

    for dev in &pci_devices {
        if dev.class_code == 0x02 {
            w.clear();
            let _ = core::write!(
                w,
                "[NET] Found NET {:02x}:{:02x}.{:x} ID {:04x}:{:04x} class {:02x}:{:02x}",
                dev.bus, dev.dev, dev.func, dev.vendor_id, dev.device_id, dev.class_code, dev.subclass
            );
            raw_fb::print(w.as_str(), raw_fb::COLOR_YELLOW, raw_fb::COLOR_DARK_BLUE);
        }
        if e1000e::probe(dev) {
            log::info!(
                "Found Intel Ethernet controller at {:02x}:{:02x}.{:x} (vendor: {:04x}, device: {:04x})",
                dev.bus,
                dev.dev,
                dev.func,
                dev.vendor_id,
                dev.device_id
            );
            w.clear();
            let _ = core::write!(w, "[NET] Calling e1000e::E1000Device::new on {:04x}:{:04x}...", dev.vendor_id, dev.device_id);
            raw_fb::print(w.as_str(), raw_fb::COLOR_CYAN, raw_fb::COLOR_DARK_BLUE);
            match e1000e::E1000Device::new(*dev) {
                Ok(nic) => {
                    raw_fb::print("[NET] Added Intel e1000e NIC OK", raw_fb::COLOR_GREEN, raw_fb::COLOR_DARK_BLUE);
                    nics.push(KernelNic::E1000(nic));
                }
                Err(err) => {
                    log::error!("Failed to initialize Intel NIC: {err}");
                    w.clear();
                    let _ = core::write!(w, "[NET] Failed Intel NIC: {err}");
                    raw_fb::print(w.as_str(), raw_fb::COLOR_RED, raw_fb::COLOR_DARK_BLUE);
                }
            }
        } else if rtl8169::probe(dev) {
            log::info!(
                "Found Realtek Ethernet controller at {:02x}:{:02x}.{:x} (vendor: {:04x}, device: {:04x})",
                dev.bus,
                dev.dev,
                dev.func,
                dev.vendor_id,
                dev.device_id
            );
            w.clear();
            let _ = core::write!(w, "[NET] Calling rtl8169::Rtl8169Device::new on {:04x}:{:04x}...", dev.vendor_id, dev.device_id);
            raw_fb::print(w.as_str(), raw_fb::COLOR_CYAN, raw_fb::COLOR_DARK_BLUE);
            match rtl8169::Rtl8169Device::new(*dev) {
                Ok(nic) => {
                    raw_fb::print("[NET] Added Realtek NIC OK", raw_fb::COLOR_GREEN, raw_fb::COLOR_DARK_BLUE);
                    nics.push(KernelNic::Rtl8169(nic));
                }
                Err(err) => {
                    log::error!("Failed to initialize Realtek NIC: {err}");
                    w.clear();
                    let _ = core::write!(w, "[NET] Failed Realtek NIC: {err}");
                    raw_fb::print(w.as_str(), raw_fb::COLOR_RED, raw_fb::COLOR_DARK_BLUE);
                }
            }
        }
    }

    if nics.is_empty() {
        let mut dev_list = alloc::string::String::new();
        for dev in &pci_devices {
            if dev.class_code == 0x02 {
                let _ = core::write!(
                    dev_list,
                    " [{:02x}:{:02x}.{:x} ID {:04x}:{:04x} class {:02x}:{:02x}]",
                    dev.bus, dev.dev, dev.func, dev.vendor_id, dev.device_id, dev.class_code, dev.subclass
                );
            }
        }
        panic!("No supported NIC matched! PCI Network devices:{dev_list}");
    }

    // Network Interface Arbitration: check link status on detected NICs, polling up to 5s
    log::info!(
        "Checking link status across {} detected NIC(s)...",
        nics.len()
    );
    w.clear();
    let _ = core::write!(w, "[NET] Found {} NIC(s). Polling link status...", nics.len());
    raw_fb::print(w.as_str(), raw_fb::COLOR_CYAN, raw_fb::COLOR_DARK_BLUE);

    let mut selected_idx = None;

    for step in 0..50 {
        for (idx, nic) in nics.iter().enumerate() {
            if nic.is_link_up() {
                selected_idx = Some(idx);
                w.clear();
                let _ = core::write!(w, "[NET] Link UP detected on NIC {} (step {})", idx, step);
                raw_fb::print(w.as_str(), raw_fb::COLOR_GREEN, raw_fb::COLOR_DARK_BLUE);
                break;
            }
        }
        if selected_idx.is_some() {
            break;
        }
        if step % 10 == 0 {
            w.clear();
            let _ = core::write!(w, "[NET] Waiting for Ethernet link... (step {})", step);
            raw_fb::print(w.as_str(), raw_fb::COLOR_YELLOW, raw_fb::COLOR_DARK_BLUE);
        }
        // Wait 100ms with bounded spin loop
        let start = Timer::micros();
        let mut iters = 0;
        while iters < 500_000 && (Timer::micros() - start) < 100_000 {
            core::hint::spin_loop();
            iters += 1;
        }
    }

    let selected_idx = selected_idx.unwrap_or_else(|| {
        log::warn!("No interface reported link up within 5s; falling back to interface 0");
        raw_fb::print("[NET] Warning: Link down after 5s; defaulting to NIC 0", raw_fb::COLOR_YELLOW, raw_fb::COLOR_DARK_BLUE);
        0
    });

    let selected_nic = nics.swap_remove(selected_idx);
    let mac = selected_nic.mac_address();
    log::info!(
        "Selected network interface with MAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} (link up: {})",
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5],
        selected_nic.is_link_up()
    );
    w.clear();
    let _ = core::write!(
        w,
        "[NET] Selected MAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} (link up: {})",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5],
        selected_nic.is_link_up()
    );
    raw_fb::print(w.as_str(), raw_fb::COLOR_GREEN, raw_fb::COLOR_DARK_BLUE);

    let hw_addr = HardwareAddress::Ethernet(smoltcp::wire::EthernetAddress::from_bytes(&mac));
    let mut device = KernelNicDevice::new(selected_nic);

    let mut interface_config = Config::new(hw_addr);
    interface_config.random_seed = rdtsc() as u64;
    let now = Timer::instant();
    let interface = Interface::new(interface_config, &mut device, now);
    let mut dhcp_socket = Dhcpv4Socket::new();
    dhcp_socket.set_outgoing_options(&[DhcpOption {
        kind: 60,
        data: b"pixie",
    }]);
    let mut socket_set = SocketSet::new(vec![]);
    let dhcp_socket_handle = socket_set.add(dhcp_socket);

    *NETWORK_DATA.lock() = Some(NetworkData {
        interface,
        device,
        socket_set,
        dhcp_socket_handle,
    });
    raw_fb::print("[NET] Smoltcp interface & DHCP socket configured OK", raw_fb::COLOR_GREEN, raw_fb::COLOR_DARK_BLUE);

    Executor::spawn("[net_poll]", async {
        loop {
            let wait = poll();
            match wait {
                Some(0) => {
                    Executor::sleep(Duration::from_micros(100)).await;
                }
                Some(us) if us < 1000 => {
                    Executor::sleep(Duration::from_micros(us.max(100))).await;
                }
                Some(us) => {
                    Executor::sleep(Duration::from_micros(us.min(5000))).await;
                }
                None => {
                    Executor::sleep(Duration::from_millis(2)).await;
                }
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

fn ip() -> Option<Ipv4Addr> {
    with_net(|n| n.interface.ipv4_addr())
}

fn get_ephemeral_port() -> u16 {
    let ans = EPHEMERAL_PORT_COUNTER.fetch_add(1, Ordering::Relaxed);
    ((ans % (60999 - 49152)) + 49152) as u16
}

/// Returns # of microseconds to wait until we should call poll() again (possibly 0), or
/// None if we can wait until the next interrupt.
fn poll() -> Option<u64> {
    let now = Timer::instant();

    let mut data = NETWORK_DATA.lock();

    let NetworkData {
        interface,
        device,
        socket_set,
        dhcp_socket_handle,
    } = data.as_mut().unwrap();

    let status_out = interface.poll_egress(now, device, socket_set);
    let mut num_ingress = 0;
    while interface.poll_ingress_single(now, device, socket_set) != PollIngressSingleResult::None {
        num_ingress += 1;
        if num_ingress >= 64 {
            break;
        }
    }

    let dhcp_status = socket_set
        .get_mut::<Dhcpv4Socket>(*dhcp_socket_handle)
        .poll();

    if let Some(dhcp_status) = dhcp_status {
        if let Event::Configured(config) = dhcp_status {
            interface.update_ip_addrs(|a| {
                a.push(IpCidr::Ipv4(config.address)).unwrap();
            });
            if let Some(router) = config.router {
                interface
                    .routes_mut()
                    .add_default_ipv4_route(router)
                    .unwrap();
            }
            let to_wake = core::mem::take(&mut *WAITING_FOR_IP.lock());
            for e in to_wake {
                e.trigger();
            }
        } else {
            interface.update_ip_addrs(|a| {
                a.clear();
            });
            interface.routes_mut().remove_default_ipv4_route();
        }
    }

    if num_ingress > 0 || status_out != PollResult::None || device.nic.has_packets() {
        return Some(0);
    }

    interface
        .poll_delay(now, socket_set)
        .map(|x| x.micros())
        .min(Some(1000))
}
