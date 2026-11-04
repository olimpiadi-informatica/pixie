//! Minimal, bounds-checked wire framing and checksums for Ethernet II, ARP, IPv4, and UDP.

use core::net::{Ipv4Addr, SocketAddrV4};

pub(crate) const ETH_HDR_LEN: usize = 14;
pub(crate) const ETHERTYPE_IPV4: u16 = 0x0800;
pub(crate) const ETHERTYPE_ARP: u16 = 0x0806;

pub(crate) const ARP_LEN: usize = 28;
pub(crate) const ARP_OP_REQUEST: u16 = 1;
pub(crate) const ARP_OP_REPLY: u16 = 2;

pub(crate) const IPV4_MIN_HDR_LEN: usize = 20;
pub(crate) const IP_PROTO_UDP: u8 = 17;

pub(crate) const UDP_HDR_LEN: usize = 8;

pub(crate) struct ArpEthIpv4 {
    pub op: u16,
    pub sender_mac: [u8; 6],
    pub sender_ip: Ipv4Addr,
    pub target_ip: Ipv4Addr,
}

pub(crate) struct Ipv4Packet<'a> {
    pub protocol: u8,
    pub src_ip: Ipv4Addr,
    pub dst_ip: Ipv4Addr,
    pub payload: &'a [u8],
}

fn ones_complement_add(sum: u32, bytes: &[u8]) -> u32 {
    // Accumulate 32-bit chunks in u64 to avoid carry overflow; 2^32 == 1 (mod 65535),
    // so summing big-endian u32 words produces the exact same 16-bit ones'-complement sum.
    let mut acc = sum as u64;
    let (chunks4, rem4) = bytes.as_chunks::<4>();
    for &chunk in chunks4 {
        acc += u32::from_be_bytes(chunk) as u64;
    }
    let (chunks2, rem2) = rem4.as_chunks::<2>();
    for &chunk in chunks2 {
        acc += u16::from_be_bytes(chunk) as u64;
    }
    if let &[last] = rem2 {
        acc += (last as u64) << 8;
    }
    acc = (acc & 0xFFFF_FFFF) + (acc >> 32);
    acc = (acc & 0xFFFF) + (acc >> 16);
    acc as u32
}

fn fold_ones_complement(mut sum: u32) -> u16 {
    while (sum >> 16) != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    sum as u16
}

fn verify_ipv4_checksum(hdr_slice: &[u8]) -> bool {
    fold_ones_complement(ones_complement_add(0, hdr_slice)) == 0xFFFF
}

fn verify_udp_checksum(src_ip: Ipv4Addr, dst_ip: Ipv4Addr, udp_slice: &[u8]) -> bool {
    let mut sum = 0u32;
    sum = ones_complement_add(sum, &src_ip.octets());
    sum = ones_complement_add(sum, &dst_ip.octets());
    sum += IP_PROTO_UDP as u32;
    sum += udp_slice.len() as u32;
    sum = ones_complement_add(sum, udp_slice);
    fold_ones_complement(sum) == 0xFFFF
}

pub(crate) fn parse_ethernet(frame: &[u8]) -> Option<([u8; 6], u16, &[u8])> {
    let hdr = frame.get(..ETH_HDR_LEN)?;
    let src_mac = hdr[6..12].try_into().ok()?;
    let ether_type = u16::from_be_bytes([hdr[12], hdr[13]]);
    Some((src_mac, ether_type, &frame[ETH_HDR_LEN..]))
}

pub(crate) fn parse_arp_eth_ipv4(payload: &[u8]) -> Option<ArpEthIpv4> {
    let pkt = payload.get(..ARP_LEN)?;
    // Ethernet (htype=1, hlen=6) + IPv4 (ptype=0x0800, plen=4)
    if pkt[0..6] != [0, 1, 0x08, 0x00, 6, 4] {
        return None;
    }
    let op = u16::from_be_bytes([pkt[6], pkt[7]]);
    let sender_mac = pkt[8..14].try_into().ok()?;
    let sender_ip = Ipv4Addr::new(pkt[14], pkt[15], pkt[16], pkt[17]);
    let target_ip = Ipv4Addr::new(pkt[24], pkt[25], pkt[26], pkt[27]);
    Some(ArpEthIpv4 {
        op,
        sender_mac,
        sender_ip,
        target_ip,
    })
}

pub(crate) fn parse_ipv4(buf: &[u8]) -> Option<Ipv4Packet<'_>> {
    if buf.len() < IPV4_MIN_HDR_LEN || (buf[0] >> 4) != 4 {
        return None;
    }
    let hdr_len = ((buf[0] & 0x0F) as usize) * 4;
    let total_len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    if hdr_len < IPV4_MIN_HDR_LEN || total_len < hdr_len || buf.len() < total_len {
        return None;
    }
    // Reject fragmented packets (MF bit 0x2000 or non-zero fragment offset 0x1FFF).
    if (u16::from_be_bytes([buf[6], buf[7]]) & 0x3FFF) != 0 {
        return None;
    }
    if !verify_ipv4_checksum(&buf[..hdr_len]) {
        return None;
    }
    Some(Ipv4Packet {
        protocol: buf[9],
        src_ip: Ipv4Addr::new(buf[12], buf[13], buf[14], buf[15]),
        dst_ip: Ipv4Addr::new(buf[16], buf[17], buf[18], buf[19]),
        payload: &buf[hdr_len..total_len],
    })
}

pub(crate) fn parse_udp(
    buf: &[u8],
    src_ip: Ipv4Addr,
    dst_ip: Ipv4Addr,
) -> Option<(u16, u16, &[u8])> {
    let hdr = buf.get(..UDP_HDR_LEN)?;
    let src_port = u16::from_be_bytes([hdr[0], hdr[1]]);
    let dst_port = u16::from_be_bytes([hdr[2], hdr[3]]);
    let udp_len = u16::from_be_bytes([hdr[4], hdr[5]]) as usize;
    let csum = u16::from_be_bytes([hdr[6], hdr[7]]);
    let udp_slice = buf.get(..udp_len)?;
    if udp_len < UDP_HDR_LEN || (csum != 0 && !verify_udp_checksum(src_ip, dst_ip, udp_slice)) {
        return None;
    }
    Some((src_port, dst_port, &udp_slice[UDP_HDR_LEN..]))
}

pub(crate) fn write_arp_eth_ipv4(
    buf: &mut [u8],
    op: u16,
    sender_mac: [u8; 6],
    sender_ip: Ipv4Addr,
    target_mac: [u8; 6],
    target_ip: Ipv4Addr,
) -> usize {
    let eth_dst = if op == ARP_OP_REQUEST {
        [0xff; 6]
    } else {
        target_mac
    };
    buf[0..6].copy_from_slice(&eth_dst);
    buf[6..12].copy_from_slice(&sender_mac);
    buf[12..14].copy_from_slice(&ETHERTYPE_ARP.to_be_bytes());
    let arp = &mut buf[ETH_HDR_LEN..ETH_HDR_LEN + ARP_LEN];
    arp[0..6].copy_from_slice(&[0, 1, 0x08, 0x00, 6, 4]);
    arp[6..8].copy_from_slice(&op.to_be_bytes());
    arp[8..14].copy_from_slice(&sender_mac);
    arp[14..18].copy_from_slice(&sender_ip.octets());
    arp[18..24].copy_from_slice(&target_mac);
    arp[24..28].copy_from_slice(&target_ip.octets());
    ETH_HDR_LEN + ARP_LEN
}

pub(crate) fn write_ipv4_udp(
    buf: &mut [u8],
    src_mac: [u8; 6],
    dst_mac: [u8; 6],
    src: SocketAddrV4,
    dst: SocketAddrV4,
    data: &[u8],
) -> usize {
    let udp_len = (UDP_HDR_LEN + data.len()) as u16;
    let total_len = ETH_HDR_LEN + IPV4_MIN_HDR_LEN + udp_len as usize;
    buf[0..6].copy_from_slice(&dst_mac);
    buf[6..12].copy_from_slice(&src_mac);
    buf[12..14].copy_from_slice(&ETHERTYPE_IPV4.to_be_bytes());

    let src_octets = src.ip().octets();
    let dst_octets = dst.ip().octets();
    let ip_end = ETH_HDR_LEN + IPV4_MIN_HDR_LEN;
    let ip = &mut buf[ETH_HDR_LEN..ip_end];
    ip[0] = 0x45; // IPv4, IHL = 5 (20 bytes)
    ip[1] = 0;
    ip[2..4].copy_from_slice(&((IPV4_MIN_HDR_LEN as u16) + udp_len).to_be_bytes());
    ip[4..6].copy_from_slice(&[0, 0]);
    ip[6..8].copy_from_slice(&[0x40, 0x00]); // Don't Fragment
    ip[8] = 64;
    ip[9] = IP_PROTO_UDP;
    ip[10..12].copy_from_slice(&[0, 0]);
    ip[12..16].copy_from_slice(&src_octets);
    ip[16..20].copy_from_slice(&dst_octets);
    let ip_csum = !fold_ones_complement(ones_complement_add(0, ip));
    ip[10..12].copy_from_slice(&ip_csum.to_be_bytes());

    let mut sum = 0u32;
    sum = ones_complement_add(sum, &src_octets);
    sum = ones_complement_add(sum, &dst_octets);
    sum += IP_PROTO_UDP as u32;
    sum += udp_len as u32;
    sum += src.port() as u32;
    sum += dst.port() as u32;
    sum += udp_len as u32;
    sum = ones_complement_add(sum, data);
    let udp_csum = match !fold_ones_complement(sum) {
        0 => 0xFFFF,
        c => c,
    };

    let udp = &mut buf[ip_end..ip_end + UDP_HDR_LEN];
    udp[0..2].copy_from_slice(&src.port().to_be_bytes());
    udp[2..4].copy_from_slice(&dst.port().to_be_bytes());
    udp[4..6].copy_from_slice(&udp_len.to_be_bytes());
    udp[6..8].copy_from_slice(&udp_csum.to_be_bytes());

    buf[ip_end + UDP_HDR_LEN..total_len].copy_from_slice(data);
    total_len
}
