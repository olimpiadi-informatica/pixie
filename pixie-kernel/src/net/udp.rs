use alloc::collections::VecDeque;
use core::future::poll_fn;
use core::net::SocketAddrV4;
use core::task::Poll;

use crate::error::Result;
use crate::net::speed::{RX_SPEED, TX_SPEED};
use crate::net::{ETH_PACKET_SIZE, UdpSocketSlot, with_net};

pub struct UdpSocket {
    slot_idx: usize,
}

impl UdpSocket {
    pub async fn bind(listen_port: Option<u16>) -> Result<UdpSocket> {
        super::wait_for_ip().await;
        let slot_idx = with_net(|net| {
            let port = listen_port.unwrap_or_else(|| net.alloc_ephemeral_port());
            let slot = UdpSocketSlot {
                port,
                queue: VecDeque::new(),
                recv_waker: None,
            };
            if let Some(idx) = net.sockets.iter().position(Option::is_none) {
                net.sockets[idx] = Some(slot);
                idx
            } else {
                net.sockets.push(Some(slot));
                net.sockets.len() - 1
            }
        });
        Ok(UdpSocket { slot_idx })
    }

    pub fn local_port(&self) -> u16 {
        with_net(|net| net.sockets[self.slot_idx].as_ref().unwrap().port)
    }

    pub async fn send_to(&self, addr: SocketAddrV4, data: &[u8]) -> Result<()> {
        with_net(|net| {
            let src_port = net.sockets[self.slot_idx].as_ref().unwrap().port;
            net.send_udp(*addr.ip(), src_port, addr.port(), data);
            TX_SPEED.add_bytes(data.len());
        });
        Ok(())
    }

    /// Receives a packet zero-copy directly from the NIC RX pool and returns the buffer slot once
    /// `f` completes.
    pub async fn recv_with<R>(&self, f: impl FnOnce(&[u8], SocketAddrV4) -> R) -> R {
        let mut f = Some(f);
        poll_fn(move |cx| {
            with_net(|net| {
                let slot = net.sockets[self.slot_idx].as_mut().unwrap();
                if let Some(pkt) = slot.queue.pop_front() {
                    RX_SPEED.add_bytes(pkt.len);
                    let ret = {
                        let payload =
                            &net.rx_pool.buf(pkt.buf_idx)[pkt.offset..pkt.offset + pkt.len];
                        f.take().unwrap()(payload, pkt.src)
                    };
                    net.rx_pool.free(pkt.buf_idx);
                    Poll::Ready(ret)
                } else {
                    slot.recv_waker = Some(cx.waker().clone());
                    Poll::Pending
                }
            })
        })
        .await
    }

    pub async fn recv_from<'a>(
        &self,
        buf: &'a mut [u8; ETH_PACKET_SIZE],
    ) -> (&'a mut [u8], SocketAddrV4) {
        let (len, addr) = self
            .recv_with(|payload, addr| {
                let n = payload.len().min(buf.len());
                buf[..n].copy_from_slice(&payload[..n]);
                (n, addr)
            })
            .await;
        (&mut buf[..len], addr)
    }
}

impl Drop for UdpSocket {
    fn drop(&mut self) {
        with_net(|net| {
            if let Some(slot) = net.sockets[self.slot_idx].take() {
                for pkt in slot.queue {
                    net.rx_pool.free(pkt.buf_idx);
                }
            }
        });
    }
}
