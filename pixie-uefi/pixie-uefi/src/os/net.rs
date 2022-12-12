use alloc::{boxed::Box, collections::BTreeMap};
use log::info;
use managed::ManagedSlice;

use smoltcp::{
    iface::{Interface, InterfaceBuilder, NeighborCache, Routes, SocketHandle, SocketSet},
    phy::{Device, DeviceCapabilities, Medium, RxToken},
    socket::dhcpv4::{Event, Socket as Dhcpv4Socket},
    wire::{HardwareAddress, IpCidr},
    Result,
};
use uefi::{
    prelude::BootServices,
    proto::network::snp::{ReceiveFlags, SimpleNetwork},
    table::boot::ScopedProtocol,
    Status,
};

use smoltcp::phy::TxToken;
use smoltcp::time::Instant;

use super::{rng::Rng, timer::Timer};

const PACKET_SIZE: usize = 1514;

type SNP = &'static ScopedProtocol<'static, SimpleNetwork>;

struct SNPDevice {
    snp: SNP,
    tx_buf: [u8; PACKET_SIZE],
    // Received packets might contain Ethernet-related padding (up to 4 bytes).
    rx_buf: [u8; PACKET_SIZE + 4],
}

impl SNPDevice {
    fn new(snp: SNP) -> SNPDevice {
        // Shut down the SNP protocol if needed.
        let _ = snp.shutdown();
        let _ = snp.stop();
        // Initialize.
        snp.start().unwrap();
        snp.initialize(0, 0).unwrap();
        // Enable packet reception.
        snp.receive_filters(
            ReceiveFlags::UNICAST | ReceiveFlags::BROADCAST,
            ReceiveFlags::empty(),
            true,
            None,
        )
        .unwrap();

        SNPDevice {
            snp,
            tx_buf: [0; PACKET_SIZE],
            rx_buf: [0; PACKET_SIZE + 4],
        }
    }
}

impl Drop for SNPDevice {
    fn drop(&mut self) {
        self.snp.stop().unwrap()
    }
}

struct SnpRxToken<'a> {
    packet: &'a mut [u8],
}

struct SnpTxToken<'a> {
    snp: SNP,
    buf: &'a mut [u8],
}

impl<'a> TxToken for SnpTxToken<'a> {
    fn consume<R, F>(self, _: Instant, len: usize, f: F) -> Result<R>
    where
        F: FnOnce(&mut [u8]) -> Result<R>,
    {
        assert!(len <= self.buf.len());
        let payload = &mut self.buf[..len];
        let ret = f(payload)?;
        let snp = self.snp;
        snp.transmit(0, payload, None, None, None)
            .expect("Failed to transmit frame");
        // Wait until sending is complete.
        while snp.get_recycled_transmit_buffer_status().unwrap().is_none() {}
        Ok(ret)
    }
}

impl<'a> RxToken for SnpRxToken<'a> {
    fn consume<R, F>(self, _: Instant, f: F) -> Result<R>
    where
        F: FnOnce(&mut [u8]) -> Result<R>,
    {
        f(self.packet)
    }
}

impl Device for SNPDevice {
    type TxToken<'d> = SnpTxToken<'d>;
    type RxToken<'d> = SnpRxToken<'d>;

    fn receive(&mut self) -> Option<(SnpRxToken<'_>, SnpTxToken<'_>)> {
        let rec = self.snp.receive(&mut self.rx_buf, None, None, None, None);
        if rec == Err(Status::NOT_READY.into()) {
            return None;
        }
        Some((
            SnpRxToken {
                packet: &mut self.rx_buf[..rec.unwrap()],
            },
            SnpTxToken {
                snp: self.snp,
                buf: &mut self.tx_buf,
            },
        ))
    }

    fn transmit(&mut self) -> Option<SnpTxToken<'_>> {
        Some(SnpTxToken {
            snp: self.snp,
            buf: &mut self.tx_buf,
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        let mode = self.snp.mode();
        assert!(mode.media_header_size == 14);
        caps.max_transmission_unit =
            PACKET_SIZE.min((mode.max_packet_size + mode.media_header_size) as usize);
        caps.max_burst_size = Some(1);
        caps
    }
}

pub struct NetworkInterface {
    interface: Interface<'static>,
    device: SNPDevice,
    socket_set: SocketSet<'static>,
    dhcp_socket_handle: SocketHandle,
    ephemeral_port_counter: u64,
}

const TCP_BUF_SIZE: usize = 1 << 22;

impl NetworkInterface {
    pub fn new(boot_services: &'static BootServices, rng: &mut Rng) -> NetworkInterface {
        let snp_handles = boot_services.find_handles::<SimpleNetwork>().unwrap();
        let snp = Box::leak(Box::new(
            boot_services
                .open_protocol_exclusive::<SimpleNetwork>(snp_handles[0])
                .unwrap(),
        ));
        let mut device = SNPDevice::new(snp);

        let routes = Routes::new(BTreeMap::new());
        let neighbor_cache = NeighborCache::new(BTreeMap::new());
        let hw_addr = HardwareAddress::Ethernet(smoltcp::wire::EthernetAddress::from_bytes(
            &snp.mode().current_address.0[..6],
        ));

        let interface = InterfaceBuilder::new()
            .hardware_addr(hw_addr)
            .routes(routes)
            .ip_addrs(vec![])
            .random_seed(rng.rand_u64())
            .neighbor_cache(neighbor_cache)
            .finalize(&mut device);

        let dhcp_socket = Dhcpv4Socket::new();
        let mut socket_set = SocketSet::new(vec![]);
        let dhcp_socket_handle = socket_set.add(dhcp_socket);

        NetworkInterface {
            interface,
            dhcp_socket_handle,
            device,
            socket_set,
            ephemeral_port_counter: rng.rand_u64(),
        }
    }

    fn get_ephemeral_port(&mut self) -> u16 {
        let ans = self.ephemeral_port_counter;
        self.ephemeral_port_counter += 1;
        ((ans % (60999 - 49152)) + 49152) as u16
    }

    pub fn has_ip(&mut self) -> bool {
        self.interface.ipv4_addr().is_some()
    }

    /*
    pub fn connect(&mut self, to: IpEndpoint) -> Result<SocketHandle> {
        let mut tcp_socket = TcpSocket::new(
            RingBuffer::new(vec![0; TCP_BUF_SIZE]),
            RingBuffer::new(vec![0; TCP_BUF_SIZE]),
        );
        tcp_socket.set_timeout(Some(Duration::from_secs(5)));
        tcp_socket.set_keep_alive(Some(Duration::from_secs(1)));
        let sport = self.get_ephemeral_port();
        tcp_socket
            .connect(self.interface.context(), to, sport)
            .unwrap();
        Ok(self.socket_set.add(tcp_socket))
    }

    pub fn tcp_socket(&mut self, socket: &SocketHandle) -> &mut TcpSocket {
        self.socket_set.get_mut::<TcpSocket>(socket)
    }

    pub fn tcp_state(&mut self, socket: &TcpSocketHandle) -> State {
        self.socket_set.get_mut::<TcpSocket>(socket.0).state()
    }

    pub fn send_tcp(&mut self, socket: &TcpSocketHandle, data: &[u8]) -> usize {
        // An error may only occur in case of incorrect usage.
        self.socket_set
            .get_mut::<TcpSocket>(socket.0)
            .send_slice(data)
            .unwrap()
    }

    pub fn stop_sending_tcp(&mut self, socket: &TcpSocketHandle) {
        self.socket_set.get_mut::<TcpSocket>(socket.0).close()
    }

    /// Returns None if connection is closed.
    pub fn recv_tcp(&mut self, socket: &TcpSocketHandle, data: &mut [u8]) -> Option<usize> {
        if self.tcp_state(socket) == State::Closed {
            return None;
        }
        let status = self
            .socket_set
            .get_mut::<TcpSocket>(socket.0)
            .recv_slice(data);
        if status == Err(RecvError::Finished) {
            None
        } else {
            // Only other failure mode is due to incorrect configuration.
            Some(status.unwrap())
        }
    }

    pub fn remove(&mut self, socket: TcpSocketHandle) {
        self.socket_set.remove(socket.0);
    }
    */

    pub(super) fn poll<F>(&mut self, timer: &Timer, on_dhcp_change: F) -> bool
    where
        F: FnOnce(),
    {
        let now = timer.instant();
        let status = self
            .interface
            .poll(now, &mut self.device, &mut self.socket_set);
        if let Err(err) = status {
            if err != smoltcp::Error::Unrecognized {
                info!("net error: {:?}", err);
            }
            return false;
        }
        let status = status.unwrap();
        if !status {
            return false;
        }

        let dhcp_status = self
            .socket_set
            .get_mut::<Dhcpv4Socket>(self.dhcp_socket_handle)
            .poll();

        if let Some(dhcp_status) = dhcp_status {
            if let Event::Configured(config) = dhcp_status {
                self.interface.update_ip_addrs(|a| {
                    if let ManagedSlice::Owned(ref mut a) = a {
                        a.push(IpCidr::Ipv4(config.address));
                    } else {
                        panic!("Invalid addresses: {:?}", a);
                    }
                });
                if let Some(router) = config.router {
                    self.interface
                        .routes_mut()
                        .add_default_ipv4_route(router)
                        .unwrap();
                }
                on_dhcp_change();
            }
        }

        return true;
    }
}
