use core::net::Ipv4Addr;

use super::checksum::{internet_checksum, udp_ipv4_checksum};

pub const ETH_HDR_LEN: usize = 14;
pub const MIN_ETH_FRAME_LEN: usize = 60;
pub const IPV4_HDR_LEN: usize = 20;
pub const UDP_HDR_LEN: usize = 8;
pub const ICMP_HDR_LEN: usize = 8;
pub const ETH_PACKET_SIZE: usize = 1514;
pub const BROADCAST_MAC: [u8; 6] = [0xFF; 6];

const ETHERTYPE_IPV4: u16 = 0x0800;
const ETHERTYPE_ARP: u16 = 0x0806;
const ETHERTYPE_VLAN: u16 = 0x8100;

pub const ARP_OP_REQUEST: u16 = 1;
pub const ARP_OP_REPLY: u16 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArpPacket {
    pub op: u16,
    pub sender_mac: [u8; 6],
    pub sender_ip: Ipv4Addr,
    pub target_mac: [u8; 6],
    pub target_ip: Ipv4Addr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RxFrame {
    Arp(ArpPacket),
    Udp {
        src_mac: [u8; 6],
        src_ip: Ipv4Addr,
        dst_ip: Ipv4Addr,
        src_port: u16,
        dst_port: u16,
        payload_offset: usize,
        payload_len: usize,
    },
    IcmpEcho {
        src_mac: [u8; 6],
        src_ip: Ipv4Addr,
        dst_ip: Ipv4Addr,
        payload_offset: usize,
        payload_len: usize,
    },
}

pub fn parse_rx_frame(
    frame: &[u8],
    my_mac: [u8; 6],
    my_ip: Option<Ipv4Addr>,
    netmask: Option<Ipv4Addr>,
) -> Option<RxFrame> {
    if frame.len() < ETH_HDR_LEN {
        return None;
    }
    let dst_mac: [u8; 6] = frame[0..6].try_into().unwrap();
    if dst_mac != my_mac && dst_mac != BROADCAST_MAC {
        return None;
    }
    let src_mac: [u8; 6] = frame[6..12].try_into().unwrap();
    let mut ethertype = u16::from_be_bytes([frame[12], frame[13]]);
    let mut l3_offset = ETH_HDR_LEN;
    if ethertype == ETHERTYPE_VLAN {
        if frame.len() < ETH_HDR_LEN + 4 {
            return None;
        }
        ethertype = u16::from_be_bytes([frame[16], frame[17]]);
        l3_offset = ETH_HDR_LEN + 4;
    }

    let l3 = &frame[l3_offset..];
    match ethertype {
        ETHERTYPE_ARP => {
            if l3.len() < 28 {
                return None;
            }
            let htype = u16::from_be_bytes([l3[0], l3[1]]);
            let ptype = u16::from_be_bytes([l3[2], l3[3]]);
            if htype != 1 || ptype != ETHERTYPE_IPV4 || l3[4] != 6 || l3[5] != 4 {
                return None;
            }
            let op = u16::from_be_bytes([l3[6], l3[7]]);
            if op != ARP_OP_REQUEST && op != ARP_OP_REPLY {
                return None;
            }
            Some(RxFrame::Arp(ArpPacket {
                op,
                sender_mac: l3[8..14].try_into().unwrap(),
                sender_ip: Ipv4Addr::new(l3[14], l3[15], l3[16], l3[17]),
                target_mac: l3[18..24].try_into().unwrap(),
                target_ip: Ipv4Addr::new(l3[24], l3[25], l3[26], l3[27]),
            }))
        }
        ETHERTYPE_IPV4 => {
            if l3.len() < IPV4_HDR_LEN {
                return None;
            }
            if l3[0] >> 4 != 4 {
                return None;
            }
            let ihl = ((l3[0] & 0x0F) as usize) * 4;
            if ihl < IPV4_HDR_LEN || l3.len() < ihl {
                return None;
            }
            let total_len = u16::from_be_bytes([l3[2], l3[3]]) as usize;
            if total_len < ihl || l3.len() < total_len {
                return None;
            }
            // Reject fragmented IPv4 packets.
            let frag_off = u16::from_be_bytes([l3[6], l3[7]]) & 0x3FFF;
            if frag_off != 0 {
                return None;
            }
            if internet_checksum(&l3[..ihl]) != 0 {
                return None;
            }

            let proto = l3[9];
            let src_ip = Ipv4Addr::new(l3[12], l3[13], l3[14], l3[15]);
            let dst_ip = Ipv4Addr::new(l3[16], l3[17], l3[18], l3[19]);
            let l4_offset = l3_offset + ihl;
            let l4 = &l3[ihl..total_len];

            match proto {
                17 => {
                    if l4.len() < UDP_HDR_LEN {
                        return None;
                    }
                    let src_port = u16::from_be_bytes([l4[0], l4[1]]);
                    let dst_port = u16::from_be_bytes([l4[2], l4[3]]);
                    let udp_len = u16::from_be_bytes([l4[4], l4[5]]) as usize;
                    if udp_len < UDP_HDR_LEN || l4.len() < udp_len {
                        return None;
                    }
                    let allowed_dst = if dst_ip == Ipv4Addr::BROADCAST {
                        true
                    } else if let (Some(ip), Some(mask)) = (my_ip, netmask) {
                        let subnet_bcast = Ipv4Addr::from(u32::from(ip) | !u32::from(mask));
                        dst_ip == ip || dst_ip == subnet_bcast
                    } else {
                        dst_port == 68
                    };
                    if !allowed_dst {
                        return None;
                    }
                    let raw_csum = u16::from_be_bytes([l4[6], l4[7]]);
                    if raw_csum != 0 && udp_ipv4_checksum(src_ip, dst_ip, &l4[..udp_len]) != 0 {
                        return None;
                    }
                    Some(RxFrame::Udp {
                        src_mac,
                        src_ip,
                        dst_ip,
                        src_port,
                        dst_port,
                        payload_offset: l4_offset + UDP_HDR_LEN,
                        payload_len: udp_len - UDP_HDR_LEN,
                    })
                }
                1 => {
                    if my_ip != Some(dst_ip) || l4.len() < ICMP_HDR_LEN {
                        return None;
                    }
                    if l4[0] != 8 || l4[1] != 0 || internet_checksum(l4) != 0 {
                        return None;
                    }
                    Some(RxFrame::IcmpEcho {
                        src_mac,
                        src_ip,
                        dst_ip,
                        payload_offset: l4_offset + 4,
                        payload_len: l4.len() - 4,
                    })
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn write_eth_hdr(buf: &mut [u8], src_mac: [u8; 6], dst_mac: [u8; 6], ethertype: u16) {
    buf[0..6].copy_from_slice(&dst_mac);
    buf[6..12].copy_from_slice(&src_mac);
    buf[12..14].copy_from_slice(&ethertype.to_be_bytes());
}

fn finish_eth_frame(buf: &mut [u8], len: usize) -> usize {
    if len < MIN_ETH_FRAME_LEN {
        buf[len..MIN_ETH_FRAME_LEN].fill(0);
        MIN_ETH_FRAME_LEN
    } else {
        len
    }
}

#[allow(clippy::too_many_arguments)]
pub fn write_arp_frame(
    buf: &mut [u8],
    src_mac: [u8; 6],
    dst_mac: [u8; 6],
    op: u16,
    sender_mac: [u8; 6],
    sender_ip: Ipv4Addr,
    target_mac: [u8; 6],
    target_ip: Ipv4Addr,
) -> usize {
    write_eth_hdr(buf, src_mac, dst_mac, ETHERTYPE_ARP);
    let arp = &mut buf[ETH_HDR_LEN..ETH_HDR_LEN + 28];
    arp[0..2].copy_from_slice(&1u16.to_be_bytes());
    arp[2..4].copy_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
    arp[4] = 6;
    arp[5] = 4;
    arp[6..8].copy_from_slice(&op.to_be_bytes());
    arp[8..14].copy_from_slice(&sender_mac);
    arp[14..18].copy_from_slice(&sender_ip.octets());
    arp[18..24].copy_from_slice(&target_mac);
    arp[24..28].copy_from_slice(&target_ip.octets());
    finish_eth_frame(buf, ETH_HDR_LEN + 28)
}

fn write_ipv4_header(
    buf: &mut [u8],
    total_len: usize,
    ip_id: u16,
    proto: u8,
    src_ip: Ipv4Addr,
    dst_ip: Ipv4Addr,
) {
    let hdr = &mut buf[ETH_HDR_LEN..ETH_HDR_LEN + IPV4_HDR_LEN];
    hdr[0] = 0x45;
    hdr[1] = 0;
    hdr[2..4].copy_from_slice(&(total_len as u16).to_be_bytes());
    hdr[4..6].copy_from_slice(&ip_id.to_be_bytes());
    hdr[6..8].copy_from_slice(&0x4000u16.to_be_bytes()); // Don't Fragment
    hdr[8] = 64; // TTL
    hdr[9] = proto;
    hdr[10..12].fill(0);
    hdr[12..16].copy_from_slice(&src_ip.octets());
    hdr[16..20].copy_from_slice(&dst_ip.octets());
    let csum = internet_checksum(hdr);
    hdr[10..12].copy_from_slice(&csum.to_be_bytes());
}

#[allow(clippy::too_many_arguments)]
pub fn write_udp_frame(
    buf: &mut [u8],
    src_mac: [u8; 6],
    dst_mac: [u8; 6],
    src_ip: Ipv4Addr,
    dst_ip: Ipv4Addr,
    src_port: u16,
    dst_port: u16,
    ip_id: u16,
    payload: &[u8],
) -> usize {
    let udp_len = UDP_HDR_LEN + payload.len();
    let ip_total = IPV4_HDR_LEN + udp_len;
    let frame_len = ETH_HDR_LEN + ip_total;
    assert!(frame_len <= buf.len());

    write_eth_hdr(buf, src_mac, dst_mac, ETHERTYPE_IPV4);
    write_ipv4_header(buf, ip_total, ip_id, 17, src_ip, dst_ip);

    let udp = &mut buf[ETH_HDR_LEN + IPV4_HDR_LEN..frame_len];
    udp[0..2].copy_from_slice(&src_port.to_be_bytes());
    udp[2..4].copy_from_slice(&dst_port.to_be_bytes());
    udp[4..6].copy_from_slice(&(udp_len as u16).to_be_bytes());
    udp[6..8].fill(0);
    udp[8..].copy_from_slice(payload);
    let csum = udp_ipv4_checksum(src_ip, dst_ip, udp);
    udp[6..8].copy_from_slice(&(if csum == 0 { 0xFFFF } else { csum }).to_be_bytes());

    finish_eth_frame(buf, frame_len)
}

pub fn write_icmp_echo_reply(
    buf: &mut [u8],
    src_mac: [u8; 6],
    dst_mac: [u8; 6],
    src_ip: Ipv4Addr,
    dst_ip: Ipv4Addr,
    ip_id: u16,
    echo_rest: &[u8],
) -> usize {
    let icmp_len = 4 + echo_rest.len();
    let ip_total = IPV4_HDR_LEN + icmp_len;
    let frame_len = ETH_HDR_LEN + ip_total;
    assert!(frame_len <= buf.len());

    write_eth_hdr(buf, src_mac, dst_mac, ETHERTYPE_IPV4);
    write_ipv4_header(buf, ip_total, ip_id, 1, src_ip, dst_ip);

    let icmp = &mut buf[ETH_HDR_LEN + IPV4_HDR_LEN..frame_len];
    icmp[0] = 0; // Echo Reply
    icmp[1] = 0;
    icmp[2..4].fill(0);
    icmp[4..].copy_from_slice(echo_rest);
    let csum = internet_checksum(icmp);
    icmp[2..4].copy_from_slice(&csum.to_be_bytes());

    finish_eth_frame(buf, frame_len)
}
