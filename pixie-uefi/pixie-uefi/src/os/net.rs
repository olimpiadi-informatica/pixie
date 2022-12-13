use core::{future::poll_fn, task::Poll};

use alloc::{boxed::Box, collections::BTreeMap};
use futures::future::select;
use log::info;
use managed::ManagedSlice;

use smoltcp::{
    iface::{Interface, InterfaceBuilder, NeighborCache, Routes, SocketHandle, SocketSet},
    phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken},
    socket::{
        dhcpv4::{Event, Socket as Dhcpv4Socket},
        tcp::{Socket as TcpSocket, State},
    },
    storage::RingBuffer,
    time::{Duration, Instant},
    wire::{HardwareAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address},
};

use uefi::{
    prelude::BootServices,
    proto::network::snp::{ReceiveFlags, SimpleNetwork},
    table::boot::ScopedProtocol,
    Status,
};

use super::{rng::Rng, timer::Timer, UefiOS};

use super::error::{Error, Result};

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
    fn consume<R, F>(self, _: Instant, len: usize, f: F) -> smoltcp::Result<R>
    where
        F: FnOnce(&mut [u8]) -> smoltcp::Result<R>,
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
    fn consume<R, F>(self, _: Instant, f: F) -> smoltcp::Result<R>
    where
        F: FnOnce(&mut [u8]) -> smoltcp::Result<R>,
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

    pub fn has_ip(&self) -> bool {
        self.interface.ipv4_addr().is_some()
    }

    pub(super) fn poll(&mut self, timer: &Timer) -> bool {
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
            } else {
                self.interface.update_ip_addrs(|a| {
                    if let ManagedSlice::Owned(ref mut a) = a {
                        a.clear();
                    } else {
                        panic!("Invalid addresses: {:?}", a);
                    }
                });
                self.interface.routes_mut().remove_default_ipv4_route();
            }
        }

        true
    }
}

pub struct TcpStream {
    handle: SocketHandle,
    os: UefiOS,
}

// TODO(veluca): we may leak a fair bit of sockets here. It doesn't really matter, as we won't
// create that many, but still it would be nice to fix eventually.
// Also, trying to use a closed connection may result in panics.
impl TcpStream {
    pub async fn new(os: UefiOS, ip: (u8, u8, u8, u8), port: u16) -> Result<TcpStream> {
        os.wait_for_ip().await;
        const TCP_BUF_SIZE: usize = 1 << 22;
        let mut tcp_socket = TcpSocket::new(
            RingBuffer::new(vec![0; TCP_BUF_SIZE]),
            RingBuffer::new(vec![0; TCP_BUF_SIZE]),
        );
        tcp_socket.set_timeout(Some(Duration::from_secs(5)));
        tcp_socket.set_keep_alive(Some(Duration::from_secs(1)));
        let sport = os.net().get_ephemeral_port();
        tcp_socket.connect(
            os.net().interface.context(),
            IpEndpoint {
                addr: IpAddress::Ipv4(Ipv4Address::new(ip.0, ip.1, ip.2, ip.3)),
                port,
            },
            sport,
        )?;

        let handle = os.net().socket_set.add(tcp_socket);

        let ret = TcpStream { handle, os };

        ret.wait_for_state(|state| match state {
            State::Established => Some(Ok(())),
            State::Closed => return Some(Err(Error::msg("connection refused"))),
            _ => None,
        })
        .await?;

        Ok(ret)
    }

    pub async fn wait_for_state<T>(&self, f: impl Fn(State) -> Option<T>) -> T {
        let handle = self.handle.clone();
        let os = self.os.clone();

        poll_fn(move |cx| {
            let state = os.net().socket_set.get_mut::<TcpSocket>(handle).state();
            let res = f(state);
            if let Some(res) = res {
                Poll::Ready(res)
            } else {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await
    }

    pub async fn wait_until_closed(&self) {
        self.wait_for_state(|s| if s == State::Closed { Some(()) } else { None })
            .await;
        self.os.net().socket_set.remove(self.handle);
    }

    async fn fail_if_closed(&self) -> Result<()> {
        self.wait_until_closed().await;
        return Err(Error::msg("connection closed"));
    }

    pub async fn send(&self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }

        let handle = self.handle.clone();
        let os = self.os.clone();

        let mut pos = 0;
        let send = poll_fn(move |cx| {
            let mut net = os.net();
            let socket = net.socket_set.get_mut::<TcpSocket>(handle);
            let sent = socket.send_slice(&data[pos..]);
            if sent.is_err() {
                return Poll::Ready(Err(Error::Send(sent.unwrap_err())));
            }
            pos += sent.unwrap();
            if pos < data.len() {
                socket.register_send_waker(cx.waker());
                Poll::Pending
            } else {
                Poll::Ready(Ok(()))
            }
        });

        select(send, Box::pin(self.fail_if_closed()))
            .await
            .factor_first()
            .0
    }

    /// Returns the number of bytes received (0 if connection is closed on the other end without
    /// receiving any data.
    pub async fn recv(&self, data: &mut [u8]) -> Result<usize> {
        let handle = self.handle.clone();
        let os = self.os.clone();

        poll_fn(move |cx| {
            let mut net = os.net();
            let socket = net.socket_set.get_mut::<TcpSocket>(handle);
            if !socket.may_recv() {
                return Poll::Ready(Ok(0));
            }
            let recvd = socket.recv_slice(data);
            if recvd == Err(smoltcp::socket::tcp::RecvError::Finished) {
                return Poll::Ready(Ok(0));
            }
            if recvd.is_err() {
                return Poll::Ready(Err(Error::Recv(recvd.unwrap_err())));
            }
            if recvd.unwrap() == 0 {
                socket.register_recv_waker(cx.waker());
                Poll::Pending
            } else {
                Poll::Ready(Ok(recvd.unwrap()))
            }
        })
        .await
    }

    pub async fn close_send(&self) {
        {
            self.os
                .net()
                .socket_set
                .get_mut::<TcpSocket>(self.handle)
                .close();
        }
        self.wait_for_state(|state| match state {
            State::Closed | State::Closing | State::FinWait1 | State::FinWait2 => Some(()),
            _ => None,
        })
        .await
    }

    pub async fn force_close(&self) {
        {
            self.os
                .net()
                .socket_set
                .get_mut::<TcpSocket>(self.handle)
                .abort();
        }
        self.wait_until_closed().await;
    }
}
