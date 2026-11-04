use alloc::boxed::Box;
use alloc::collections::VecDeque;
use core::future::poll_fn;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::pin::pin;
use core::sync::atomic::{AtomicU16, AtomicU32, Ordering};
use core::task::Poll;
use core::time::Duration;

use futures::future::{Either, select};
use pixie_shared::{
    DeserializeOwned, RpcRequest, UDP_BODY_LEN, UPLOAD_SLICE_LEN, UPLOAD_WINDOW_SLICES, UdpRequest,
};

use crate::error::{Error, Result};
use crate::executor::Executor;
use crate::net::speed::{RX_SPEED, TX_SPEED};
use crate::net::wire;
use crate::net::{ETH_PACKET_SIZE, UdpSocketState, with_net};
use crate::timer::rdtsc;

static NEXT_RPC_SEQ: AtomicU32 = AtomicU32::new(0);
static NEXT_UPLOAD_ID: AtomicU16 = AtomicU16::new(0);

fn next_rpc_seq() -> u32 {
    loop {
        let cur = NEXT_RPC_SEQ.load(Ordering::Relaxed);
        if cur == 0 {
            let t = rdtsc() as u64;
            let seed = ((t ^ (t >> 21) ^ (t >> 42)) as u32).max(1);
            let _ = NEXT_RPC_SEQ.compare_exchange(0, seed, Ordering::Relaxed, Ordering::Relaxed);
            continue;
        }
        let next = cur.wrapping_add(1).max(1);
        if NEXT_RPC_SEQ
            .compare_exchange(cur, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return cur;
        }
    }
}

fn next_upload_id() -> u16 {
    loop {
        let cur = NEXT_UPLOAD_ID.load(Ordering::Relaxed);
        if cur == 0 {
            let t = rdtsc() as u64;
            let seed = ((t ^ (t >> 16) ^ (t >> 32)) as u16).max(1);
            let _ = NEXT_UPLOAD_ID.compare_exchange(0, seed, Ordering::Relaxed, Ordering::Relaxed);
            continue;
        }
        let next = cur.wrapping_add(1).max(1);
        if NEXT_UPLOAD_ID
            .compare_exchange(cur, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return cur;
        }
    }
}

pub struct UdpSocket {
    port: u16,
}

impl UdpSocket {
    pub(super) fn bind_raw(port: u16) -> UdpSocket {
        with_net(|n| {
            n.udp_sockets.retain(|(p, _)| *p != port);
            n.udp_sockets.push((
                port,
                UdpSocketState {
                    rx_queue: VecDeque::new(),
                    rx_waker: None,
                },
            ));
        });
        UdpSocket { port }
    }

    pub async fn bind(listen_port: Option<u16>) -> Result<UdpSocket> {
        super::wait_for_ip().await;
        let sport = listen_port.unwrap_or_else(super::get_ephemeral_port);
        Ok(Self::bind_raw(sport))
    }

    pub async fn send_to(&self, addr: SocketAddrV4, data: &[u8]) -> Result<()> {
        if data.len() > UDP_BODY_LEN {
            return Err(Error::msg("UDP payload exceeds MTU"));
        }

        let dst_ip = *addr.ip();
        let dst_mac = loop {
            let resolved = with_net(|net| {
                if dst_ip == Ipv4Addr::BROADCAST || net.is_subnet_broadcast(dst_ip) {
                    return Ok([0xff; 6]);
                }
                let hop = net.next_hop(dst_ip);
                if let Some(mac) = net.lookup_arp(hop) {
                    Ok(mac)
                } else {
                    Err(hop)
                }
            });
            match resolved {
                Ok(mac) => break mac,
                Err(hop) => {
                    with_net(|net| net.send_arp(wire::ARP_OP_REQUEST, [0; 6], hop));
                    let wait_arp = Box::pin(poll_fn(|cx| {
                        with_net(|net| {
                            if net.lookup_arp(hop).is_some() {
                                Poll::Ready(())
                            } else {
                                net.arp_wakers.push(cx.waker().clone());
                                Poll::Pending
                            }
                        })
                    }));
                    let timeout = Box::pin(Executor::sleep(Duration::from_millis(200)));
                    if matches!(select(wait_arp, timeout).await, Either::Right(_)) {
                        with_net(|net| {
                            for waker in net.arp_wakers.drain(..) {
                                waker.wake();
                            }
                        });
                    }
                }
            }
        };

        let sport = self.port;
        loop {
            let sent = with_net(|net| {
                if !net.device.nic.can_transmit() {
                    return false;
                }
                let src_mac = net.mac;
                let src = SocketAddrV4::new(net.ip.unwrap_or(Ipv4Addr::UNSPECIFIED), sport);
                net.device.transmit_with(|buf| {
                    wire::write_ipv4_udp(buf, src_mac, dst_mac, src, addr, data)
                });
                true
            });
            if sent {
                break;
            }
            Executor::sched_yield().await;
        }
        TX_SPEED.add_bytes(data.len());
        Ok(())
    }

    pub async fn recv_from<'a>(
        &self,
        buf: &'a mut [u8; ETH_PACKET_SIZE],
    ) -> (&'a mut [u8], SocketAddrV4) {
        let (pkt, addr) = poll_fn(|cx| {
            with_net(|net| {
                let (_, sock) = net
                    .udp_sockets
                    .iter_mut()
                    .find(|(p, _)| *p == self.port)
                    .unwrap();
                if let Some(item) = sock.rx_queue.pop_front() {
                    Poll::Ready(item)
                } else {
                    sock.rx_waker = Some(cx.waker().clone());
                    Poll::Pending
                }
            })
        })
        .await;

        let len = pkt.len().min(buf.len());
        buf[..len].copy_from_slice(&pkt[..len]);
        RX_SPEED.add_bytes(len);

        (&mut buf[..len], addr)
    }

    async fn recv_from_polling<'a>(
        &self,
        buf: &'a mut [u8; ETH_PACKET_SIZE],
    ) -> (&'a mut [u8], SocketAddrV4) {
        let (pkt, addr) = loop {
            while super::poll() {}
            let item = with_net(|net| {
                net.udp_sockets
                    .iter_mut()
                    .find(|(p, _)| *p == self.port)
                    .and_then(|(_, sock)| sock.rx_queue.pop_front())
            });
            if let Some(item) = item {
                break item;
            }
            Executor::sched_yield().await;
        };

        let len = pkt.len().min(buf.len());
        buf[..len].copy_from_slice(&pkt[..len]);
        RX_SPEED.add_bytes(len);

        (&mut buf[..len], addr)
    }

    pub async fn rpc<T: DeserializeOwned>(
        &self,
        server: SocketAddrV4,
        req: RpcRequest,
    ) -> Result<T> {
        let seq = next_rpc_seq();
        let mut pkt_buf = [0u8; UDP_BODY_LEN];
        let packet = postcard::to_slice(&UdpRequest::Rpc { seq, req }, &mut pkt_buf)?;
        let mut buf = [0u8; ETH_PACKET_SIZE];
        let mut retry_ms = 50u64;
        loop {
            self.send_to(server, packet).await?;
            let mut timeout = Box::pin(Executor::sleep(Duration::from_millis(retry_ms)));
            loop {
                let recv = pin!(self.recv_from_polling(&mut buf));
                match select(recv, timeout).await {
                    Either::Left(((data, addr), next_timeout)) => {
                        timeout = next_timeout;
                        if addr == server
                            && let Ok((reply_seq, resp)) = postcard::from_bytes::<(u32, T)>(data)
                            && reply_seq == seq
                        {
                            return Ok(resp);
                        }
                    }
                    Either::Right(((), _)) => {
                        retry_ms = (retry_ms * 2).min(250);
                        break;
                    }
                }
            }
        }
    }

    fn window_mask(window_len: usize) -> u64 {
        let count = window_len.div_ceil(UPLOAD_SLICE_LEN);
        if count == 0 {
            0
        } else {
            u64::MAX >> (64 - count)
        }
    }

    async fn send_upload_window(
        &self,
        server: SocketAddrV4,
        id: u16,
        base: usize,
        window: &[u8],
        missing: u64,
        pkt_buf: &mut [u8; UDP_BODY_LEN],
    ) -> Result<()> {
        for (i, slice) in window.chunks(UPLOAD_SLICE_LEN).enumerate() {
            if (missing & (1u64 << i)) != 0 {
                let req = UdpRequest::UploadSlice {
                    id,
                    offset: (base + i * UPLOAD_SLICE_LEN) as u32,
                    data: slice,
                };
                let pkt = postcard::to_slice(&req, pkt_buf)?;
                self.send_to(server, pkt).await?;
            }
        }
        Ok(())
    }

    async fn finish_upload_window(
        &self,
        server: SocketAddrV4,
        id: u16,
        base: usize,
        window: &[u8],
        pkt_buf: &mut [u8; UDP_BODY_LEN],
    ) -> Result<()> {
        let count = window.len().div_ceil(UPLOAD_SLICE_LEN) as u8;
        let mut missing = Self::window_mask(window.len());
        while missing != 0 {
            let ack: u64 = self
                .rpc(
                    server,
                    RpcRequest::UploadSync {
                        id,
                        base: base as u32,
                        count,
                    },
                )
                .await?;
            missing &= !ack;
            if missing != 0 {
                self.send_upload_window(server, id, base, window, missing, pkt_buf)
                    .await?;
            }
        }
        Ok(())
    }

    pub async fn upload_blob(&self, server: SocketAddrV4, data: &[u8]) -> Result<()> {
        let id = next_upload_id();
        let window_bytes = UPLOAD_WINDOW_SLICES * UPLOAD_SLICE_LEN;
        let mut pkt_buf = [0u8; UDP_BODY_LEN];
        let mut prev: Option<(usize, &[u8])> = None;

        for (window_idx, window) in data.chunks(window_bytes).enumerate() {
            let base = window_idx * window_bytes;
            let all = Self::window_mask(window.len());
            // Pipeline up to two windows: push the next window before awaiting UploadSync for the previous one.
            self.send_upload_window(server, id, base, window, all, &mut pkt_buf)
                .await?;
            if let Some((prev_base, prev_window)) = prev.replace((base, window)) {
                self.finish_upload_window(server, id, prev_base, prev_window, &mut pkt_buf)
                    .await?;
            }
        }
        if let Some((last_base, last_window)) = prev {
            self.finish_upload_window(server, id, last_base, last_window, &mut pkt_buf)
                .await?;
        }
        Ok(())
    }
}

impl Drop for UdpSocket {
    fn drop(&mut self) {
        with_net(|net| {
            net.udp_sockets.retain(|(p, _)| *p != self.port);
        })
    }
}
