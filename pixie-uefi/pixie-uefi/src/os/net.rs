use alloc::{collections::BTreeMap};
use log::info;
use managed::ManagedSlice;

use smoltcp::{
    iface::{Interface, InterfaceBuilder, NeighborCache, Routes, SocketHandle, SocketSet},
    phy::{Device, DeviceCapabilities, Medium, RxToken},
    socket::tcp::{Socket as TcpSocket, State},
    socket::{
        dhcpv4::{Event, Socket as Dhcpv4Socket},
        tcp::RecvError,
    },
    storage::RingBuffer,
    time::Duration,
    wire::{HardwareAddress, IpCidr, IpEndpoint},
    Result,
};
use uefi::{
    proto::network::snp::{ReceiveFlags},
    Status,
};

use smoltcp::phy::TxToken;
use smoltcp::time::Instant;

use super::{timer::get_time_micros, UefiOS};

struct SNPDevice {
    os: UefiOS,
}

impl SNPDevice {
    fn new(os: UefiOS) -> SNPDevice {
        let _ = os.simple_network().shutdown();
        let _ = os.simple_network().stop();
        os.simple_network().start().unwrap();
        os.simple_network().initialize(0, 0).unwrap();
        os.simple_network()
            .receive_filters(
                ReceiveFlags::UNICAST | ReceiveFlags::BROADCAST,
                ReceiveFlags::empty(),
                true,
                None,
            )
            .unwrap();

        SNPDevice { os }
    }
}

impl Drop for SNPDevice {
    fn drop(&mut self) {
        self.os.simple_network().stop().unwrap()
    }
}

const PACKET_SIZE: usize = 1514;

struct SnpRxToken {
    packet: [u8; PACKET_SIZE + 4],
    len: usize,
}

struct SnpTxToken {
    os: UefiOS,
}

impl TxToken for SnpTxToken {
    fn consume<R, F>(self, _: Instant, len: usize, f: F) -> Result<R>
    where
        F: FnOnce(&mut [u8]) -> Result<R>,
    {
        let mut buf = [0u8; PACKET_SIZE];
        assert!(len <= PACKET_SIZE);
        let payload = &mut buf[..len];

        let ret = f(payload)?;

        let snp = self.os.simple_network();

        snp.transmit(0, payload, None, None, None)
            .expect("Failed to transmit frame");

        // Wait until sending is complete.
        while snp.get_recycled_transmit_buffer_status().unwrap().is_none() {}
        Ok(ret)
    }
}

impl RxToken for SnpRxToken {
    fn consume<R, F>(mut self, _: Instant, f: F) -> Result<R>
    where
        F: FnOnce(&mut [u8]) -> Result<R>,
    {
        let packet = &mut self.packet[..self.len];
        f(packet)
    }
}

impl Device for SNPDevice {
    type TxToken<'d> = SnpTxToken;
    type RxToken<'d> = SnpRxToken;

    fn receive(&mut self) -> Option<(SnpRxToken, SnpTxToken)> {
        // Ethernet frames may have some extra padding (up to 4 bytes).
        let mut buffer = [0u8; PACKET_SIZE + 4];
        self.os.simple_network().get_interrupt_status().unwrap();
        let rec = self
            .os
            .simple_network()
            .receive(&mut buffer, None, None, None, None);
        if rec == Err(Status::NOT_READY.into()) {
            None
        } else {
            let token = SnpRxToken {
                packet: buffer,
                len: rec.unwrap(),
            };
            Some((token, SnpTxToken { os: self.os }))
        }
    }

    fn transmit(&mut self) -> Option<SnpTxToken> {
        Some(SnpTxToken { os: self.os })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = PACKET_SIZE.min(
            (self.os.simple_network().mode().max_packet_size
                + self.os.simple_network().mode().media_header_size) as usize,
        );
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

pub struct TcpSocketHandle(SocketHandle);

const TCP_BUF_SIZE: usize = 1 << 16;

impl NetworkInterface {
    fn get_ephemeral_port(&mut self) -> u16 {
        let ans = self.ephemeral_port_counter;
        self.ephemeral_port_counter += 1;
        ((ans % (60999 - 49152)) + 49152) as u16
    }

    pub fn new(os: UefiOS) -> NetworkInterface {
        let mut device = SNPDevice::new(os);

        let routes = Routes::new(BTreeMap::new());
        let neighbor_cache = NeighborCache::new(BTreeMap::new());
        let hw_addr = HardwareAddress::Ethernet(smoltcp::wire::EthernetAddress::from_bytes(
            &os.simple_network().mode().current_address.0[..6],
        ));

        let interface = InterfaceBuilder::new()
            .hardware_addr(hw_addr)
            .routes(routes)
            .ip_addrs(vec![])
            .random_seed(os.rand_u64())
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
            ephemeral_port_counter: os.rand_u64(),
        }
    }

    pub fn has_ip(&mut self) -> bool {
        self.interface.ipv4_addr().is_some()
    }

    pub fn connect(&mut self, to: IpEndpoint) -> Result<TcpSocketHandle> {
        let mut tcp_socket = TcpSocket::new(
            RingBuffer::new(vec![0; TCP_BUF_SIZE]),
            RingBuffer::new(vec![0; TCP_BUF_SIZE]),
        );
        tcp_socket.set_timeout(Some(Duration::from_secs(5)));
        let sport = self.get_ephemeral_port();
        tcp_socket
            .connect(self.interface.context(), to, sport)
            .unwrap();
        Ok(TcpSocketHandle(self.socket_set.add(tcp_socket)))
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

    // TODO: figure out why this sends a RST.
    pub fn remove(&mut self, socket: TcpSocketHandle) {
        self.socket_set.remove(socket.0);
    }

    pub fn poll(&mut self) -> bool {
        let status = self.interface.poll(
            Instant::from_micros(get_time_micros()),
            &mut self.device,
            &mut self.socket_set,
        );
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
            info!("{:?}", dhcp_status);
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
            }
        }

        return true;
    }
}
