use alloc::boxed::Box;
use core::future::{poll_fn, Future};
use core::net::SocketAddrV4;
use core::task::Poll;

use futures::future::select;
use smoltcp::iface::SocketHandle;
use smoltcp::socket::tcp::{Socket as TcpSocket, State};
use smoltcp::storage::RingBuffer;
use smoltcp::time::Duration;
use smoltcp::wire::IpEndpoint;

use crate::os::error::{Error, Result};
use crate::os::net::speed::{RX_SPEED, TX_SPEED};
use crate::os::net::with_net;

pub struct TcpStream {
    handle: SocketHandle,
}

// TODO(veluca): we may leak a fair bit of sockets here. It doesn't really matter, as we won't
// create that many, but still it would be nice to fix eventually.
// Also, trying to use a closed connection may result in panics.
impl TcpStream {
    pub async fn connect(addr: SocketAddrV4) -> Result<TcpStream> {
        super::wait_for_ip().await;
        const TCP_BUF_SIZE: usize = 1 << 22;
        let mut tcp_socket = TcpSocket::new(
            RingBuffer::new(vec![0; TCP_BUF_SIZE]),
            RingBuffer::new(vec![0; TCP_BUF_SIZE]),
        );
        tcp_socket.set_congestion_control(smoltcp::socket::tcp::CongestionControl::Cubic);
        tcp_socket.set_timeout(Some(Duration::from_secs(5)));
        tcp_socket.set_keep_alive(Some(Duration::from_secs(1)));
        let sport = super::get_ephemeral_port();
        let handle = with_net(|net| {
            tcp_socket.connect(
                net.interface.context(),
                IpEndpoint {
                    addr: (*addr.ip()).into(),
                    port: addr.port(),
                },
                sport,
            )?;

            Ok::<_, Error>(net.socket_set.add(tcp_socket))
        })?;

        let ret = TcpStream { handle };

        ret.wait_for_state(|state| match state {
            State::Established => Poll::Ready(Ok(())),
            State::Closed => Poll::Ready(Err(Error::msg("connection refused"))),
            _ => Poll::Pending,
        })
        .await?;
        Ok(ret)
    }

    fn wait_for_state<'a, T>(
        &'a self,
        f: impl Fn(State) -> Poll<T> + 'a,
    ) -> impl Future<Output = T> + 'a {
        poll_fn(move |cx| {
            let state = with_net(|n| n.socket_set.get_mut::<TcpSocket>(self.handle).state());
            let res = f(state);
            if matches!(res, Poll::Pending) {
                cx.waker().wake_by_ref();
            }
            res
        })
    }

    async fn wait_until_closed(&self) {
        self.wait_for_state(|s| {
            if s == State::Closed {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
        with_net(|n| n.socket_set.remove(self.handle));
    }

    async fn fail_if_closed(&self) -> Result<()> {
        self.wait_until_closed().await;
        Err(Error::msg("connection closed"))
    }

    pub async fn write_all(&self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }

        let mut pos = 0;
        let send = poll_fn(move |cx| {
            with_net(|net| {
                let socket = net.socket_set.get_mut::<TcpSocket>(self.handle);
                let sent = socket.send_slice(&data[pos..]);
                if let Err(err) = sent {
                    return Poll::Ready(Err(err.into()));
                }
                pos += sent.unwrap();
                TX_SPEED.add_bytes(sent.unwrap());
                if pos < data.len() {
                    socket.register_send_waker(cx.waker());
                    Poll::Pending
                } else {
                    Poll::Ready(Ok(()))
                }
            })
        });

        select(send, Box::pin(self.fail_if_closed()))
            .await
            .factor_first()
            .0
    }

    /// Returns the number of bytes received (0 if connection is closed on the other end without
    /// receiving any data.
    pub fn read<'a>(&'a self, data: &'a mut [u8]) -> impl Future<Output = Result<usize>> + 'a {
        poll_fn(move |cx| {
            with_net(|net| {
                let socket = net.socket_set.get_mut::<TcpSocket>(self.handle);
                if !socket.may_recv() {
                    return Poll::Ready(Ok(0));
                }
                let recvd = socket.recv_slice(data);
                if recvd == Err(smoltcp::socket::tcp::RecvError::Finished) {
                    return Poll::Ready(Ok(0));
                }
                if let Err(err) = recvd {
                    return Poll::Ready(Err(err.into()));
                }
                if recvd.unwrap() == 0 {
                    socket.register_recv_waker(cx.waker());
                    Poll::Pending
                } else {
                    RX_SPEED.add_bytes(recvd.unwrap());
                    Poll::Ready(Ok(recvd.unwrap()))
                }
            })
        })
    }

    pub async fn read_exact(&self, data: &mut [u8]) -> Result<()> {
        let mut pos = 0;
        while pos < data.len() {
            let recvd = self.read(&mut data[pos..]).await?;
            if recvd == 0 {
                return Err(Error::msg("connection closed"));
            }
            pos += recvd;
        }
        Ok(())
    }

    pub async fn write_u64_le(&self, data: u64) -> Result<()> {
        self.write_all(&data.to_le_bytes()).await
    }

    pub async fn read_u64_le(&self) -> Result<u64> {
        let mut buf = [0; 8];
        self.read_exact(&mut buf).await?;
        Ok(u64::from_le_bytes(buf))
    }

    pub async fn shutdown(&self) {
        with_net(|n| {
            n.socket_set.get_mut::<TcpSocket>(self.handle).close();
        });
        self.wait_for_state(|state| match state {
            State::Closed | State::Closing | State::FinWait1 | State::FinWait2 => Poll::Ready(()),
            _ => Poll::Pending,
        })
        .await
    }

    pub async fn force_close(self) {
        with_net(|n| n.socket_set.get_mut::<TcpSocket>(self.handle).abort());
        self.wait_until_closed().await;
    }
}
