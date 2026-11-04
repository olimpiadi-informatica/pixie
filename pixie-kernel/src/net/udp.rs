use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::future::poll_fn;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::sync::atomic::{AtomicU16, AtomicU32, Ordering};
use core::task::Poll;
use core::time::Duration;

use etherparse::{ArpOperation, EtherType, Ethernet2Header, IpNumber, Ipv4Header, UdpHeader};
use futures::future::{Either, select};
use pixie_shared::{DeserializeOwned, RpcRequest, UDP_BODY_LEN, UPLOAD_SLICE_LEN, UdpRequest};

use crate::error::{Error, Result};
use crate::executor::Executor;
use crate::net::speed::{RX_SPEED, TX_SPEED};
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
                    with_net(|net| net.send_arp(ArpOperation::REQUEST, [0; 6], hop));
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
        let dport = addr.port();
        loop {
            let sent = with_net(|net| {
                if !net.device.nic.can_transmit() {
                    return false;
                }
                let src_ip = net.ip.unwrap_or(Ipv4Addr::UNSPECIFIED).octets();
                let eth = Ethernet2Header {
                    source: net.mac,
                    destination: dst_mac,
                    ether_type: EtherType::IPV4,
                }
                .to_bytes();
                let Ok(mut ip) = Ipv4Header::new(
                    (UdpHeader::LEN + data.len()) as u16,
                    64,
                    IpNumber::UDP,
                    src_ip,
                    dst_ip.octets(),
                ) else {
                    unreachable!()
                };
                ip.header_checksum = ip.calc_header_checksum();
                let Ok(udp) = UdpHeader::with_ipv4_checksum(sport, dport, &ip, data) else {
                    unreachable!()
                };
                let ip_bytes = ip.to_bytes();
                let udp_bytes = udp.to_bytes();
                net.device.transmit_with(|buf| {
                    let mut pos = 0;
                    buf[pos..pos + eth.len()].copy_from_slice(&eth);
                    pos += eth.len();
                    buf[pos..pos + ip_bytes.len()].copy_from_slice(&ip_bytes);
                    pos += ip_bytes.len();
                    buf[pos..pos + udp_bytes.len()].copy_from_slice(&udp_bytes);
                    pos += udp_bytes.len();
                    buf[pos..pos + data.len()].copy_from_slice(data);
                    pos + data.len()
                });
                true
            });
            if sent {
                break;
            }
            Executor::wait_for_interrupt().await;
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

    pub async fn rpc<T: DeserializeOwned>(
        &self,
        server: SocketAddrV4,
        req: RpcRequest,
    ) -> Result<T> {
        let seq = next_rpc_seq();
        let packet = postcard::to_allocvec(&UdpRequest::Rpc { seq, req })?;
        let mut buf = [0u8; ETH_PACKET_SIZE];
        loop {
            self.send_to(server, &packet).await?;
            let mut timeout = Box::pin(Executor::sleep(Duration::from_millis(250)));
            loop {
                let recv = Box::pin(self.recv_from(&mut buf));
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
                    Either::Right(((), _)) => break,
                }
            }
        }
    }

    pub async fn upload_blob(&self, server: SocketAddrV4, data: &[u8]) -> Result<()> {
        let id = next_upload_id();
        for (window_idx, window) in data.chunks(32 * UPLOAD_SLICE_LEN).enumerate() {
            let base = window_idx * 32 * UPLOAD_SLICE_LEN;
            let slices: Vec<&[u8]> = window.chunks(UPLOAD_SLICE_LEN).collect();
            let count = slices.len() as u8;
            let mut missing: u32 = if count == 32 {
                u32::MAX
            } else {
                (1u32 << count) - 1
            };
            while missing != 0 {
                for (i, slice) in slices.iter().enumerate() {
                    if (missing & (1 << i)) != 0 {
                        let req = UdpRequest::UploadSlice {
                            id,
                            offset: (base + i * UPLOAD_SLICE_LEN) as u32,
                            data: (*slice).to_vec(),
                        };
                        let pkt = postcard::to_allocvec(&req)?;
                        self.send_to(server, &pkt).await?;
                    }
                }
                let ack: u32 = self
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
            }
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
