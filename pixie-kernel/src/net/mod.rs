use core::fmt::Write;
use core::net::Ipv4Addr;
use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;

use smoltcp::iface::{
    Config, Interface, PollIngressSingleResult, PollResult, SocketHandle, SocketSet,
};
use smoltcp::socket::dhcpv4;
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpCidr};
use spin::Mutex;
use uefi::proto::console::text::Color;

use crate::executor::Executor;
use crate::executor::event::{Event, EventTrigger};
use crate::net::interface::KernelNicDevice;
pub use crate::net::tcp::TcpStream;
pub use crate::net::udp::UdpSocket;
use crate::timer::Timer;
use crate::ui;

pub mod interface;
pub mod speed;
pub mod tcp;
pub mod udp;

pub const ETH_PACKET_SIZE: usize = 1514;

static EPHEMERAL_PORT_COUNTER: AtomicU64 = AtomicU64::new(0);

#[allow(dead_code)]
pub(crate) struct NetworkData {
    pub interface: Interface,
    pub device: KernelNicDevice,
    pub socket_set: SocketSet<'static>,
    pub dhcp_socket_handle: SocketHandle,
}

pub(crate) static NETWORK_DATA: Mutex<Option<NetworkData>> = Mutex::new(None);
pub(crate) static WAITING_FOR_IP: Mutex<alloc::vec::Vec<EventTrigger>> =
    Mutex::new(alloc::vec::Vec::new());

pub(crate) fn with_net<T, F: FnOnce(&mut NetworkData) -> T>(f: F) -> T {
    let mut mg = NETWORK_DATA.try_lock().expect("Network is locked");
    f(mg.as_mut().expect("Network is not initialized"))
}

pub fn init() {
    use crate::hal::NativeHal;
    use crate::net::interface::KernelNic;
    use pixie_drivers::net::e1000::{self, E1000Device};
    use pixie_drivers::net::rtl::{self, RtlDevice};
    use pixie_drivers::pci::{Bar, scan_pci};

    let fallback_mac = crate::boot_info::BOOT_INFO.lock().and_then(|b| b.uefi_mac);
    let pci_devs = scan_pci(&NativeHal);
    let mut found_nic = None;

    for &pci in &pci_devs {
        if e1000::probe(&pci) {
            for bar_idx in 0..6 {
                if let Some(Bar::Memory { base, .. }) = pci.read_bar(&NativeHal, bar_idx) {
                    crate::arch::paging::map_mmio(base, 0x20000);
                    if let Ok(dev) = E1000Device::new(pci, base, fallback_mac, NativeHal) {
                        log::info!(
                            "Initialized Intel e1000/e1000e NIC (MAC {:02x?})",
                            dev.mac_address()
                        );
                        found_nic = Some(KernelNic::E1000(dev));
                        break;
                    }
                }
            }
            if found_nic.is_some() {
                break;
            }
        }
    }

    if found_nic.is_none() {
        for &pci in &pci_devs {
            if rtl::probe(&pci).is_some() {
                let mut reg_bar = None;
                for bar_idx in 0..6 {
                    if let Some(Bar::Memory { base, .. }) = pci.read_bar(&NativeHal, bar_idx) {
                        reg_bar = Some((base, false));
                        break;
                    }
                }
                if reg_bar.is_none() {
                    for bar_idx in 0..6 {
                        if let Some(Bar::Io { port }) = pci.read_bar(&NativeHal, bar_idx) {
                            reg_bar = Some((port as u64, true));
                            break;
                        }
                    }
                }
                if let Some((reg_base, is_io)) = reg_bar {
                    if !is_io {
                        crate::arch::paging::map_mmio(reg_base, 0x10000);
                    }
                    if let Ok(dev) = RtlDevice::new(pci, reg_base, is_io, fallback_mac, NativeHal) {
                        log::info!("Initialized Realtek NIC (MAC {:02x?})", dev.mac_address());
                        found_nic = Some(KernelNic::Rtl(dev));
                        break;
                    }
                }
            }
        }
    }

    let nic = found_nic.expect(
        "No supported network interface found (Intel e1000/e1000e or Realtek RTL8168/8125)",
    );

    let mac = nic.mac_address();
    let mut device = KernelNicDevice {
        nic,
        tx_buf: [0u8; ETH_PACKET_SIZE],
        rx_buf: [0u8; ETH_PACKET_SIZE + 4],
    };

    let hw_addr = HardwareAddress::Ethernet(EthernetAddress(mac));
    let mut config = Config::new(hw_addr);
    config.random_seed = Timer::micros() as u64;

    let interface = Interface::new(config, &mut device, Instant::from_micros(Timer::micros()));
    let mut socket_set = SocketSet::new(alloc::vec![]);
    let dhcp_socket = dhcpv4::Socket::new();
    let dhcp_socket_handle = socket_set.add(dhcp_socket);

    *NETWORK_DATA.lock() = Some(NetworkData {
        interface,
        device,
        socket_set,
        dhcp_socket_handle,
    });

    Executor::spawn("[net_poll]", async {
        loop {
            let wait = poll();
            match wait {
                None => {
                    Executor::wait_for_interrupt().await;
                }
                Some(wait) if wait < 50 => {
                    Executor::sched_yield().await;
                }
                Some(wait) => {
                    futures::future::select(
                        Executor::wait_for_interrupt(),
                        Executor::sleep(Duration::from_micros(wait)),
                    )
                    .await;
                }
            }
        }
    });

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

pub fn ip() -> Option<Ipv4Addr> {
    with_net(|net| net.interface.ipv4_addr())
}

pub(crate) fn get_ephemeral_port() -> u16 {
    let ans = EPHEMERAL_PORT_COUNTER.fetch_add(1, Ordering::Relaxed);
    ((ans % (60999 - 49152)) + 49152) as u16
}

fn poll() -> Option<u64> {
    let now = Instant::from_micros(Timer::micros());

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
        .get_mut::<dhcpv4::Socket>(*dhcp_socket_handle)
        .poll();

    if let Some(dhcp_status) = dhcp_status {
        if let dhcpv4::Event::Configured(config) = dhcp_status {
            log::info!("DHCP configured: IP {}", config.address);
            interface.update_ip_addrs(|a| {
                a.clear();
                a.push(IpCidr::Ipv4(config.address)).unwrap();
            });
            if let Some(router) = config.router {
                interface
                    .routes_mut()
                    .add_default_ipv4_route(router)
                    .unwrap();
            } else {
                interface.routes_mut().remove_default_ipv4_route();
            }
            for e in WAITING_FOR_IP.lock().drain(..) {
                e.trigger();
            }
        } else {
            log::info!("DHCP lost configuration");
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
}
