use core::{future::poll_fn, task::Poll};

use alloc::{
    boxed::Box,
    collections::BTreeMap,
    string::{String, ToString},
    vec::Vec,
};
use futures::future::{select, try_join};
use log::info;
use managed::ManagedSlice;

use smoltcp::{
    iface::{Interface, InterfaceBuilder, NeighborCache, Routes, SocketHandle, SocketSet},
    phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken},
    socket::{
        dhcpv4::{Event, Socket as Dhcpv4Socket},
        tcp::{Socket as TcpSocket, State},
        udp,
        udp::Socket as UdpSocket,
    },
    storage::RingBuffer,
    time::{Duration, Instant},
    wire::{HardwareAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address},
};

use uefi::{
    proto::network::snp::{ReceiveFlags, SimpleNetwork},
    table::boot::ScopedProtocol,
    Status,
};

use pixie_shared::Address;

use super::error::{Error, Result};
use super::{timer::Timer, UefiOS};

pub const PACKET_SIZE: usize = 1514;

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
    pub fn new(os: UefiOS) -> NetworkInterface {
        let bo = os.boot_options();
        let curopt = bo.get(bo.current());
        let (descr, device) = bo.boot_entry_info(&curopt[..]);
        info!(
            "Configuring network on interface used for booting ({} -- {})",
            descr,
            os.device_path_to_string(device)
        );
        let mut device = SNPDevice::new(Box::leak(Box::new(
            os.open_protocol_on_device::<SimpleNetwork>(device).unwrap(),
        )));

        let routes = Routes::new(BTreeMap::new());
        let neighbor_cache = NeighborCache::new(BTreeMap::new());
        let hw_addr = HardwareAddress::Ethernet(smoltcp::wire::EthernetAddress::from_bytes(
            &device.snp.mode().current_address.0[..6],
        ));

        let interface = InterfaceBuilder::new()
            .hardware_addr(hw_addr)
            .routes(routes)
            .ip_addrs(vec![])
            .random_seed(os.rng().rand_u64())
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
            ephemeral_port_counter: os.rng().rand_u64(),
        }
    }

    fn get_ephemeral_port(&mut self) -> u16 {
        let ans = self.ephemeral_port_counter;
        self.ephemeral_port_counter += 1;
        ((ans % (60999 - 49152)) + 49152) as u16
    }

    pub fn has_ip(&self) -> bool {
        self.ip().is_some()
    }

    pub fn ip(&self) -> Option<Ipv4Address> {
        self.interface.ipv4_addr()
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
                return Poll::Ready(Err(Error::TcpSend(sent.unwrap_err())));
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

pub struct UdpHandle {
    handle: SocketHandle,
    os: UefiOS,
}

impl UdpHandle {
    pub async fn new(os: UefiOS, listen_port: Option<u16>) -> Result<UdpHandle> {
        os.wait_for_ip().await;
        const UDP_BUF_SIZE: usize = 1 << 22;
        const UDP_PACKET_BUF_SIZE: usize = 1 << 10;
        let rx_buffer = udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; UDP_PACKET_BUF_SIZE],
            vec![0; UDP_BUF_SIZE],
        );
        let tx_buffer = udp::PacketBuffer::new(
            vec![udp::PacketMetadata::EMPTY; UDP_PACKET_BUF_SIZE],
            vec![0; UDP_BUF_SIZE],
        );

        let mut udp_socket = UdpSocket::new(rx_buffer, tx_buffer);
        let sport = if let Some(p) = listen_port {
            p
        } else {
            os.net().get_ephemeral_port()
        };
        udp_socket.bind(sport)?;

        let handle = os.net().socket_set.add(udp_socket);

        let ret = UdpHandle { handle, os };
        Ok(ret)
    }

    pub async fn send(&self, ip: (u8, u8, u8, u8), port: u16, data: &[u8]) -> Result<()> {
        let endpoint = IpEndpoint {
            addr: IpAddress::Ipv4(Ipv4Address::new(ip.0, ip.1, ip.2, ip.3)),
            port,
        };

        let handle = self.handle.clone();
        let os = self.os.clone();

        Ok(poll_fn(move |cx| {
            let mut net = os.net();
            let socket = net.socket_set.get_mut::<UdpSocket>(handle);
            if !socket.can_send() {
                socket.register_send_waker(cx.waker());
                Poll::Pending
            } else {
                let status = socket.send_slice(data, endpoint);
                Poll::Ready(status)
            }
        })
        .await?)
    }

    pub async fn recv<'a>(&self, buf: &'a mut [u8; PACKET_SIZE]) -> (&'a mut [u8], Address) {
        let handle = self.handle.clone();
        let os = self.os.clone();

        let buf2 = &mut *buf;
        let (len, addr) = poll_fn(move |cx| {
            let mut net = os.net();
            let socket = net.socket_set.get_mut::<UdpSocket>(handle);
            if !socket.can_recv() {
                socket.register_recv_waker(cx.waker());
                Poll::Pending
            } else {
                // Cannot fail if can_recv() returned true.
                let recvd = socket.recv_slice(buf2).unwrap();
                let ip = (recvd.1).addr.as_bytes();
                let ip = (ip[0], ip[1], ip[2], ip[3]);
                let port = (recvd.1).port;
                Poll::Ready((recvd.0, Address { ip, port }))
            }
        })
        .await;

        (&mut buf[..len], addr)
    }

    pub fn close(self) {
        self.os
            .net()
            .socket_set
            .get_mut::<UdpSocket>(self.handle)
            .close();
        self.os.net().socket_set.remove(self.handle);
    }
}

pub enum HttpMethod<'a> {
    Get,
    Post(&'a [u8]),
}

pub async fn http<'a>(
    os: UefiOS,
    ip: (u8, u8, u8, u8),
    port: u16,
    method: HttpMethod<'a>,
    path: &[u8],
) -> Result<Vec<u8>> {
    let tcp = TcpStream::new(os, ip, port).await?;

    let send_req = async {
        match method {
            HttpMethod::Get => {
                tcp.send(b"GET ").await?;
                tcp.send(path).await?;
                tcp.send(b" HTTP/1.0\r\n\r\n").await?;
            }
            HttpMethod::Post(data) => {
                tcp.send(b"POST ").await?;
                tcp.send(path).await?;
                tcp.send(
                    &format!(" HTTP/1.0\r\nContent-Length: {}\r\n\r\n", data.len()).as_bytes(),
                )
                .await?;
                tcp.send(data).await?;
            }
        }
        Result::Ok(())
    };

    let mut resp = vec![0; 1024];
    let recv_resp = async {
        let mut recv_so_far = 0;
        loop {
            if resp.len() < recv_so_far * 2 {
                resp.resize(resp.len() * 2, 0);
            }
            let recv = tcp.recv(&mut resp[recv_so_far..]).await?;
            if recv == 0 {
                resp.resize(recv_so_far, 0xFF);
                break;
            }
            recv_so_far += recv;
        }
        Result::Ok(())
    };

    try_join(send_req, recv_resp).await?;

    tcp.close_send().await;
    tcp.wait_until_closed().await;

    // TODO(veluca): better parsing of HTTP headers.
    let end_first_line = resp
        .windows(2)
        .enumerate()
        .find_map(|b| if b.1 == b"\r\n" { Some(b.0) } else { None })
        .ok_or_else(|| Error::Generic("HTTP response has no \\r\\n".into()))?;

    if &resp[..end_first_line] != b"HTTP/1.0 200 OK" {
        return Err(Error::Generic(
            "HTTP response first line was unexpected, found ".to_string()
                + &String::from_utf8_lossy(&resp[..end_first_line]),
        ));
    }

    let end_headers = resp
        .windows(4)
        .enumerate()
        .find_map(|b| if b.1 == b"\r\n\r\n" { Some(b.0) } else { None })
        .ok_or_else(|| Error::Generic("HTTP response has no \\r\\n\\r\\n".into()))?;

    Ok(resp[end_headers + 4..].to_vec())
}
