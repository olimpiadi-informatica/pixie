use core::net::Ipv4Addr;

#[inline]
fn fold_u64(mut sum: u64) -> u16 {
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

#[inline]
fn sum_bytes(data: &[u8]) -> u64 {
    let mut sum = 0u64;
    let (chunks4, rem4) = data.as_chunks::<4>();
    for &c in chunks4 {
        sum += u32::from_be_bytes(c) as u64;
    }
    let (chunks2, rem2) = rem4.as_chunks::<2>();
    for &c in chunks2 {
        sum += u16::from_be_bytes(c) as u64;
    }
    if let &[last] = rem2 {
        sum += (last as u64) << 8;
    }
    sum
}

pub fn internet_checksum(data: &[u8]) -> u16 {
    fold_u64(sum_bytes(data))
}

pub fn udp_ipv4_checksum(src_ip: Ipv4Addr, dst_ip: Ipv4Addr, udp_segment: &[u8]) -> u16 {
    let s = src_ip.octets();
    let d = dst_ip.octets();
    let len = udp_segment.len() as u16;
    let mut sum = 0u64;
    sum += u16::from_be_bytes([s[0], s[1]]) as u64;
    sum += u16::from_be_bytes([s[2], s[3]]) as u64;
    sum += u16::from_be_bytes([d[0], d[1]]) as u64;
    sum += u16::from_be_bytes([d[2], d[3]]) as u64;
    sum += 17u64; // UDP protocol number
    sum += len as u64;
    sum += sum_bytes(udp_segment);
    fold_u64(sum)
}
