use core::future::{poll_fn, Future};
use core::net::{IpAddr, SocketAddrV4};
use core::task::Poll;

use smoltcp::iface::SocketHandle;
use smoltcp::socket::udp::{self, Socket};
use smoltcp::wire::IpEndpoint;

use crate::os::error::Result;
use crate::os::net::speed::{RX_SPEED, TX_SPEED};
use crate::os::net::{with_net, ETH_PACKET_SIZE};

pub struct UdpSocket {
    handle: SocketHandle,
}

impl UdpSocket {
    pub async fn bind(listen_port: Option<u16>) -> Result<UdpSocket> {
        super::wait_for_ip().await;
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

        let mut udp_socket = Socket::new(rx_buffer, tx_buffer);
        let sport = listen_port.unwrap_or_else(super::get_ephemeral_port);
        udp_socket.bind(sport)?;

        let handle = with_net(|n| n.socket_set.add(udp_socket));

        Ok(UdpSocket { handle })
    }

    pub fn send_to<'a>(
        &'a self,
        addr: SocketAddrV4,
        data: &'a [u8],
    ) -> impl Future<Output = Result<()>> + 'a {
        let endpoint = IpEndpoint {
            addr: (*addr.ip()).into(),
            port: addr.port(),
        };

        poll_fn(move |cx| {
            with_net(|net| {
                let socket = net.socket_set.get_mut::<Socket>(self.handle);
                if !socket.can_send() {
                    socket.register_send_waker(cx.waker());
                    Poll::Pending
                } else {
                    let status = socket.send_slice(data, endpoint);
                    TX_SPEED.add_bytes(data.len());
                    Poll::Ready(status.map_err(|e| e.into()))
                }
            })
        })
    }

    pub async fn recv_from<'a>(
        &self,
        buf: &'a mut [u8; ETH_PACKET_SIZE],
    ) -> (&'a mut [u8], SocketAddrV4) {
        let buf2 = &mut *buf;
        let (len, addr) = poll_fn(move |cx| {
            with_net(|net| {
                let socket = net.socket_set.get_mut::<Socket>(self.handle);
                if !socket.can_recv() {
                    socket.register_recv_waker(cx.waker());
                    Poll::Pending
                } else {
                    // Cannot fail if can_recv() returned true.
                    let recvd = socket.recv_slice(buf2).unwrap();
                    let IpAddr::V4(ip) = (recvd.1).endpoint.addr.into() else {
                        unreachable!();
                    };
                    let port = (recvd.1).endpoint.port;
                    Poll::Ready((recvd.0, SocketAddrV4::new(ip, port)))
                }
            })
        })
        .await;

        RX_SPEED.add_bytes(len);

        (&mut buf[..len], addr)
    }
}

impl Drop for UdpSocket {
    fn drop(&mut self) {
        with_net(|net| {
            net.socket_set.get_mut::<Socket>(self.handle).close();
            net.socket_set.remove(self.handle);
        })
    }
}
