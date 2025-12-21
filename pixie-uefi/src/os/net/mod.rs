use alloc::string::{String, ToString};
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
use uefi::proto::device_path::build::DevicePathBuilder;
use uefi::proto::device_path::text::{AllowShortcuts, DevicePathToText, DisplayOnly};
use uefi::proto::device_path::DevicePath;
use uefi::proto::network::snp::SimpleNetwork;
use uefi::proto::Protocol;
use uefi::Handle;

use super::timer::Timer;
use crate::os::boot_options::BootOptions;
use crate::os::executor::event::{Event as ExecutorEvent, EventTrigger};
use crate::os::executor::Executor;
use crate::os::net::interface::SnpDevice;
pub use crate::os::net::tcp::TcpStream;
pub use crate::os::net::udp::UdpSocket;
use crate::os::send_wrapper::SendWrapper;
use crate::os::timer::rdtsc;
use crate::os::ui;

mod interface;
mod speed;
mod tcp;
mod udp;

pub const ETH_PACKET_SIZE: usize = 1514;

static EPHEMERAL_PORT_COUNTER: AtomicU64 = AtomicU64::new(0);

struct NetworkData {
    interface: Interface,
    device: SnpDevice,
    socket_set: SocketSet<'static>,
    dhcp_socket_handle: SocketHandle,
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

    let hw_addr = HardwareAddress::Ethernet(smoltcp::wire::EthernetAddress::from_bytes(
        &snp.mode().current_address.0[..6],
    ));

    let mut device = SnpDevice::new(SendWrapper(snp));

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

    Executor::spawn("[net_poll]", async {
        loop {
            const MIN_WAIT_US: u64 = 1000;
            let wait = poll();
            match wait {
                None => {
                    Executor::wait_for_interrupt().await;
                }
                Some(wait) if wait < MIN_WAIT_US => {
                    // Immediately wake if we want call poll() again in a very short time.
                    Executor::sched_yield().await;
                }
                Some(wait) => {
                    futures::future::select(
                        Executor::wait_for_interrupt(),
                        // Reduce the waiting time, to try to ensure that we don't exceed the
                        // suggested waiting time.
                        Executor::sleep(Duration::from_micros(wait - MIN_WAIT_US)),
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
    let status_in = interface.poll_ingress_single(now, device, socket_set);

    if status_in == PollIngressSingleResult::None && status_out == PollResult::None {
        return interface.poll_delay(now, socket_set).map(|x| x.micros());
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
    Some(0)
}
