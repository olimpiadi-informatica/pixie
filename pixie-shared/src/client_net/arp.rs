use core::net::Ipv4Addr;

const ARP_CAPACITY: usize = 16;
const ARP_TTL_US: i64 = 300_000_000;
const ARP_RETRY_US: i64 = 250_000;

#[derive(Debug, Clone, Copy)]
struct ArpEntry {
    ip: Ipv4Addr,
    mac: [u8; 6],
    expires_us: i64,
}

#[derive(Debug, Default)]
pub struct ArpCache {
    entries: [Option<ArpEntry>; ARP_CAPACITY],
    cursor: usize,
    last_req: Option<(Ipv4Addr, i64)>,
}

impl ArpCache {
    pub fn next_hop(
        my_ip: Ipv4Addr,
        netmask: Ipv4Addr,
        router: Option<Ipv4Addr>,
        dst_ip: Ipv4Addr,
    ) -> Ipv4Addr {
        let mask = u32::from(netmask);
        if (u32::from(my_ip) & mask) == (u32::from(dst_ip) & mask) {
            dst_ip
        } else {
            router.unwrap_or(dst_ip)
        }
    }

    pub fn learn(&mut self, ip: Ipv4Addr, mac: [u8; 6], now_us: i64) {
        if ip.is_unspecified() || ip.is_broadcast() || mac[0] & 1 != 0 {
            return;
        }
        let expires_us = now_us + ARP_TTL_US;
        for slot in &mut self.entries {
            if let Some(entry) = slot
                && entry.ip == ip
            {
                entry.mac = mac;
                entry.expires_us = expires_us;
                return;
            }
        }
        for slot in &mut self.entries {
            if slot.is_none() || slot.is_some_and(|e| e.expires_us <= now_us) {
                *slot = Some(ArpEntry {
                    ip,
                    mac,
                    expires_us,
                });
                return;
            }
        }
        self.entries[self.cursor] = Some(ArpEntry {
            ip,
            mac,
            expires_us,
        });
        self.cursor = (self.cursor + 1) % ARP_CAPACITY;
    }

    pub fn lookup(&self, ip: Ipv4Addr, now_us: i64) -> Option<[u8; 6]> {
        self.entries
            .iter()
            .flatten()
            .find_map(|entry| (entry.ip == ip && entry.expires_us > now_us).then_some(entry.mac))
    }

    pub fn should_request(&mut self, ip: Ipv4Addr, now_us: i64) -> bool {
        if let Some((last_ip, last_ts)) = self.last_req
            && last_ip == ip
            && now_us - last_ts < ARP_RETRY_US
        {
            return false;
        }
        self.last_req = Some((ip, now_us));
        true
    }

    pub fn clear(&mut self) {
        self.entries = [None; ARP_CAPACITY];
        self.last_req = None;
    }
}
