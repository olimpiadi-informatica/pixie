pub mod arp;
pub mod checksum;
pub mod dhcp;
pub mod packet;

pub use arp::ArpCache;
pub use dhcp::{DhcpClient, DhcpConfig, DhcpEvent};
pub use packet::{
    ArpPacket, BROADCAST_MAC, ETH_HDR_LEN, ETH_PACKET_SIZE, IPV4_HDR_LEN, MIN_ETH_FRAME_LEN,
    RxFrame, UDP_HDR_LEN, parse_rx_frame, write_arp_frame, write_icmp_echo_reply, write_udp_frame,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_net::checksum::internet_checksum;
    use crate::client_net::packet::{ARP_OP_REPLY, ARP_OP_REQUEST};
    use alloc::vec;
    use core::net::Ipv4Addr;

    const CLIENT_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
    const SERVER_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0xAA, 0xBB, 0xCC];
    const CLIENT_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 1, 1);
    const SERVER_IP: Ipv4Addr = Ipv4Addr::new(10, 0, 0, 1);
    const MASK: Ipv4Addr = Ipv4Addr::new(255, 255, 0, 0);

    #[test]
    fn test_rfc1071_checksum() {
        // RFC 1071 Section 3 example: 0x0001 + 0xf203 + ... -> ~0x220d? Verify self-check property:
        // writing ~sum into checksum field makes internet_checksum == 0.
        let mut hdr = [
            0x45, 0x00, 0x00, 0x73, 0x00, 0x00, 0x40, 0x00, 0x40, 0x11, 0x00, 0x00, 0xc0, 0xa8,
            0x00, 0x01, 0xc0, 0xa8, 0x00, 0xc7,
        ];
        let csum = internet_checksum(&hdr);
        assert_eq!(csum, 0xb861);
        hdr[10..12].copy_from_slice(&csum.to_be_bytes());
        assert_eq!(internet_checksum(&hdr), 0);
    }

    #[test]
    fn test_udp_arp_icmp_roundtrip_and_rejection() {
        let mut buf = [0u8; ETH_PACKET_SIZE];

        // UDP frame roundtrip + VLAN tag skip
        let n = write_udp_frame(
            &mut buf, SERVER_MAC, CLIENT_MAC, SERVER_IP, CLIENT_IP, 25640, 50000, 1, b"hello",
        );
        assert_eq!(n, MIN_ETH_FRAME_LEN);
        match parse_rx_frame(&buf[..n], CLIENT_MAC, Some(CLIENT_IP), Some(MASK)).unwrap() {
            RxFrame::Udp {
                src_mac,
                src_ip,
                dst_ip,
                src_port,
                dst_port,
                payload_offset,
                payload_len,
            } => {
                assert_eq!(src_mac, SERVER_MAC);
                assert_eq!(src_ip, SERVER_IP);
                assert_eq!(dst_ip, CLIENT_IP);
                assert_eq!(src_port, 25640);
                assert_eq!(dst_port, 50000);
                assert_eq!(&buf[payload_offset..payload_offset + payload_len], b"hello");
            }
            other => panic!("unexpected {other:?}"),
        }

        // Insert 802.1Q VLAN tag (0x8100, VID=10)
        let mut vlan_buf = [0u8; ETH_PACKET_SIZE];
        vlan_buf[0..12].copy_from_slice(&buf[0..12]);
        vlan_buf[12..16].copy_from_slice(&[0x81, 0x00, 0x00, 0x0A]);
        vlan_buf[16..16 + (n - 12)].copy_from_slice(&buf[12..n]);
        assert!(matches!(
            parse_rx_frame(&vlan_buf[..n + 4], CLIENT_MAC, Some(CLIENT_IP), Some(MASK)),
            Some(RxFrame::Udp { payload_len: 5, .. })
        ));

        // Corrupt UDP checksum -> rejected
        let mut bad_udp = buf;
        bad_udp[ETH_HDR_LEN + IPV4_HDR_LEN + 6] ^= 0xFF;
        assert!(parse_rx_frame(&bad_udp[..n], CLIENT_MAC, Some(CLIENT_IP), Some(MASK)).is_none());

        // Set IP fragment offset -> rejected
        let mut frag = buf;
        frag[ETH_HDR_LEN + 7] = 1;
        frag[ETH_HDR_LEN + 10..ETH_HDR_LEN + 12].fill(0);
        let c = internet_checksum(&frag[ETH_HDR_LEN..ETH_HDR_LEN + IPV4_HDR_LEN]);
        frag[ETH_HDR_LEN + 10..ETH_HDR_LEN + 12].copy_from_slice(&c.to_be_bytes());
        assert!(parse_rx_frame(&frag[..n], CLIENT_MAC, Some(CLIENT_IP), Some(MASK)).is_none());

        // ARP roundtrip
        let an = write_arp_frame(
            &mut buf,
            SERVER_MAC,
            BROADCAST_MAC,
            ARP_OP_REQUEST,
            SERVER_MAC,
            SERVER_IP,
            [0; 6],
            CLIENT_IP,
        );
        assert_eq!(an, MIN_ETH_FRAME_LEN);
        assert_eq!(
            parse_rx_frame(&buf[..an], CLIENT_MAC, Some(CLIENT_IP), Some(MASK)).unwrap(),
            RxFrame::Arp(ArpPacket {
                op: ARP_OP_REQUEST,
                sender_mac: SERVER_MAC,
                sender_ip: SERVER_IP,
                target_mac: [0; 6],
                target_ip: CLIENT_IP,
            })
        );

        // ICMP echo request -> reply roundtrip
        let mut echo_req = [0u8; 64];
        echo_req[0..6].copy_from_slice(&CLIENT_MAC);
        echo_req[6..12].copy_from_slice(&SERVER_MAC);
        echo_req[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        // Build reply via write_icmp_echo_reply, then flip type to 8 (request) and recompute checksum
        let rn = write_icmp_echo_reply(
            &mut echo_req,
            SERVER_MAC,
            CLIENT_MAC,
            SERVER_IP,
            CLIENT_IP,
            5,
            b"\x12\x34\x00\x01ping",
        );
        let icmp_off = ETH_HDR_LEN + IPV4_HDR_LEN;
        echo_req[icmp_off] = 8;
        echo_req[icmp_off + 2..icmp_off + 4].fill(0);
        let ic = internet_checksum(&echo_req[icmp_off..icmp_off + 12]);
        echo_req[icmp_off + 2..icmp_off + 4].copy_from_slice(&ic.to_be_bytes());
        match parse_rx_frame(&echo_req[..rn], CLIENT_MAC, Some(CLIENT_IP), Some(MASK)).unwrap() {
            RxFrame::IcmpEcho {
                payload_offset,
                payload_len,
                ..
            } => {
                assert_eq!(
                    &echo_req[payload_offset..payload_offset + payload_len],
                    b"\x12\x34\x00\x01ping"
                );
            }
            other => panic!("unexpected {other:?}"),
        }
        let _ = ARP_OP_REPLY;
    }

    #[test]
    fn test_arp_cache_and_gateway() {
        let mut arp = ArpCache::default();
        let gw = Ipv4Addr::new(10, 0, 0, 254);
        let remote = Ipv4Addr::new(192, 168, 50, 10);

        assert_eq!(
            ArpCache::next_hop(CLIENT_IP, MASK, Some(gw), SERVER_IP),
            SERVER_IP
        );
        assert_eq!(ArpCache::next_hop(CLIENT_IP, MASK, Some(gw), remote), gw);

        assert!(arp.should_request(gw, 1_000));
        assert!(!arp.should_request(gw, 100_000));
        assert!(arp.should_request(gw, 300_000));

        arp.learn(gw, SERVER_MAC, 300_000);
        assert_eq!(arp.lookup(gw, 300_000), Some(SERVER_MAC));
        arp.clear();
        assert_eq!(arp.lookup(gw, 300_000), None);
    }

    fn make_dhcp_reply(
        xid: u32,
        mac: [u8; 6],
        msg_type: u8,
        yiaddr: Ipv4Addr,
        server_id: Ipv4Addr,
        lease_secs: u32,
    ) -> alloc::vec::Vec<u8> {
        let mut pkt = vec![0u8; 280];
        pkt[0] = 2; // BOOTREPLY
        pkt[4..8].copy_from_slice(&xid.to_be_bytes());
        pkt[16..20].copy_from_slice(&yiaddr.octets());
        pkt[28..34].copy_from_slice(&mac);
        pkt[236..240].copy_from_slice(&[0x63, 0x82, 0x53, 0x63]);
        let mut p = 240;
        pkt[p..p + 3].copy_from_slice(&[53, 1, msg_type]);
        p += 3;
        pkt[p..p + 6].copy_from_slice(&[
            54,
            4,
            server_id.octets()[0],
            server_id.octets()[1],
            server_id.octets()[2],
            server_id.octets()[3],
        ]);
        p += 6;
        pkt[p..p + 6].copy_from_slice(&[1, 4, 255, 255, 255, 0]);
        p += 6;
        pkt[p..p + 6].copy_from_slice(&[3, 4, 192, 168, 99, 2]);
        p += 6;
        pkt[p..p + 2].copy_from_slice(&[51, 4]);
        pkt[p + 2..p + 6].copy_from_slice(&lease_secs.to_be_bytes());
        p += 6;
        pkt[p] = 255;
        pkt
    }

    #[test]
    fn test_dhcp_dora_proxy_ignore_nak_and_renew() {
        let xid = 0x1234_5678;
        let mut now = 1_000_000i64;
        let mut dhcp = DhcpClient::new(CLIENT_MAC, xid, now);
        let mut buf = [0u8; 512];

        // 1. Discover emitted with Option 60 = "pixie"
        let (dst, len) = dhcp.poll_tx(now, &mut buf).unwrap();
        assert_eq!(dst, Ipv4Addr::BROADCAST);
        assert!(buf[240..len].windows(5).any(|w| w == b"pixie"));

        // 2. ProxyDHCP OFFER (yiaddr = 0.0.0.0) must be ignored!
        let proxy_offer = make_dhcp_reply(
            xid,
            CLIENT_MAC,
            2,
            Ipv4Addr::UNSPECIFIED,
            Ipv4Addr::new(192, 168, 99, 1),
            3600,
        );
        assert_eq!(dhcp.on_rx(&proxy_offer, now), None);
        assert!(dhcp.config().is_none());

        // 3. Real OFFER -> Requesting -> REQUEST -> ACK -> Bound
        let real_ip = Ipv4Addr::new(192, 168, 99, 50);
        let real_sid = Ipv4Addr::new(192, 168, 99, 2);
        let real_offer = make_dhcp_reply(xid, CLIENT_MAC, 2, real_ip, real_sid, 60);
        assert_eq!(dhcp.on_rx(&real_offer, now), None);

        let (dst, _) = dhcp.poll_tx(now, &mut buf).unwrap();
        assert_eq!(dst, Ipv4Addr::BROADCAST);

        let ack = make_dhcp_reply(xid, CLIENT_MAC, 5, real_ip, real_sid, 60);
        let ev = dhcp.on_rx(&ack, now);
        assert!(
            matches!(ev, Some(DhcpEvent::Bound(cfg)) if cfg.ip == real_ip && cfg.server_id == real_sid)
        );

        // 4. Advance past T1 (30s) -> unicast Renewing -> ACK
        now += 31_000_000;
        let (dst, _) = dhcp.poll_tx(now, &mut buf).unwrap();
        assert_eq!(dst, real_sid);
        assert_eq!(dhcp.on_rx(&ack, now), None); // same config

        // 5. Advance to T1 again, receive NAK -> Deconfigured
        now += 31_000_000;
        let _ = dhcp.poll_tx(now, &mut buf).unwrap();
        let nak = make_dhcp_reply(xid, CLIENT_MAC, 6, Ipv4Addr::UNSPECIFIED, real_sid, 60);
        assert_eq!(dhcp.on_rx(&nak, now), Some(DhcpEvent::Deconfigured));
        assert!(dhcp.config().is_none());
    }
}
