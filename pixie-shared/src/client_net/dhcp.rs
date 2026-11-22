use core::net::Ipv4Addr;

const COOKIE: [u8; 4] = [0x63, 0x82, 0x53, 0x63];
const MSG_DISCOVER: u8 = 1;
const MSG_OFFER: u8 = 2;
const MSG_REQUEST: u8 = 3;
const MSG_ACK: u8 = 5;
const MSG_NAK: u8 = 6;
const DEFAULT_LEASE_SECS: u32 = 3600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DhcpConfig {
    pub ip: Ipv4Addr,
    pub netmask: Ipv4Addr,
    pub router: Option<Ipv4Addr>,
    pub server_id: Ipv4Addr,
    pub lease_secs: u32,
}

impl DhcpConfig {
    pub fn broadcast_ip(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.ip) | !u32::from(self.netmask))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DhcpEvent {
    Bound(DhcpConfig),
    Deconfigured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Discovering,
    Requesting {
        offered: DhcpConfig,
    },
    Bound {
        t1_us: i64,
        t2_us: i64,
        expire_us: i64,
    },
    Renewing {
        t2_us: i64,
        expire_us: i64,
    },
}

#[derive(Debug)]
pub struct DhcpClient {
    mac: [u8; 6],
    xid: u32,
    state: State,
    config: Option<DhcpConfig>,
    next_tx_us: i64,
    retries: u32,
}

impl DhcpClient {
    pub fn new(mac: [u8; 6], xid: u32, now_us: i64) -> Self {
        Self {
            mac,
            xid,
            state: State::Discovering,
            config: None,
            next_tx_us: now_us,
            retries: 0,
        }
    }

    pub fn config(&self) -> Option<&DhcpConfig> {
        self.config.as_ref()
    }

    fn backoff_us(retries: u32) -> i64 {
        (1_000_000i64) << retries.min(2)
    }

    fn write_packet(
        &self,
        out_buf: &mut [u8],
        msg_type: u8,
        ciaddr: Ipv4Addr,
        broadcast_flag: bool,
        req_ip_and_sid: Option<(Ipv4Addr, Ipv4Addr)>,
    ) -> usize {
        out_buf[..240].fill(0);
        out_buf[0] = 1; // BOOTREQUEST
        out_buf[1] = 1; // Ethernet
        out_buf[2] = 6; // HLEN
        out_buf[4..8].copy_from_slice(&self.xid.to_be_bytes());
        if broadcast_flag {
            out_buf[10..12].copy_from_slice(&0x8000u16.to_be_bytes());
        }
        out_buf[12..16].copy_from_slice(&ciaddr.octets());
        out_buf[28..34].copy_from_slice(&self.mac);
        out_buf[236..240].copy_from_slice(&COOKIE);

        let mut p = 240;
        out_buf[p..p + 3].copy_from_slice(&[53, 1, msg_type]);
        p += 3;
        if let Some((ip, sid)) = req_ip_and_sid {
            out_buf[p..p + 2].copy_from_slice(&[50, 4]);
            out_buf[p + 2..p + 6].copy_from_slice(&ip.octets());
            p += 6;
            out_buf[p..p + 2].copy_from_slice(&[54, 4]);
            out_buf[p + 2..p + 6].copy_from_slice(&sid.octets());
            p += 6;
        }
        // Option 55: Subnet Mask (1), Router (3)
        out_buf[p..p + 4].copy_from_slice(&[55, 2, 1, 3]);
        p += 4;
        // Option 60: Vendor Class Identifier = "pixie"
        out_buf[p..p + 7].copy_from_slice(&[60, 5, b'p', b'i', b'x', b'i', b'e']);
        p += 7;
        out_buf[p] = 255;
        p + 1
    }

    pub fn poll_tx(&mut self, now_us: i64, out_buf: &mut [u8]) -> Option<(Ipv4Addr, usize)> {
        if let State::Bound {
            t1_us,
            t2_us,
            expire_us,
        } = self.state
        {
            if now_us >= t1_us {
                self.state = State::Renewing { t2_us, expire_us };
                self.next_tx_us = now_us;
                self.retries = 0;
            } else {
                return None;
            }
        }

        if let State::Renewing { expire_us, .. } = self.state
            && now_us >= expire_us
        {
            self.state = State::Discovering;
            self.config = None;
            self.next_tx_us = now_us;
            self.retries = 0;
        }

        if now_us < self.next_tx_us {
            return None;
        }

        match self.state {
            State::Discovering => {
                let len =
                    self.write_packet(out_buf, MSG_DISCOVER, Ipv4Addr::UNSPECIFIED, true, None);
                self.next_tx_us = now_us + Self::backoff_us(self.retries);
                self.retries = self.retries.saturating_add(1);
                Some((Ipv4Addr::BROADCAST, len))
            }
            State::Requesting { offered } => {
                if self.retries >= 4 {
                    self.state = State::Discovering;
                    self.retries = 0;
                    self.next_tx_us = now_us;
                    return None;
                }
                let len = self.write_packet(
                    out_buf,
                    MSG_REQUEST,
                    Ipv4Addr::UNSPECIFIED,
                    true,
                    Some((offered.ip, offered.server_id)),
                );
                self.next_tx_us = now_us + Self::backoff_us(self.retries);
                self.retries += 1;
                Some((Ipv4Addr::BROADCAST, len))
            }
            State::Renewing { t2_us, .. } => {
                let cfg = self.config?;
                let len = self.write_packet(out_buf, MSG_REQUEST, cfg.ip, false, None);
                self.next_tx_us = now_us + 5_000_000;
                self.retries = self.retries.saturating_add(1);
                let dst = if now_us < t2_us {
                    cfg.server_id
                } else {
                    Ipv4Addr::BROADCAST
                };
                Some((dst, len))
            }
            State::Bound { .. } => None,
        }
    }

    pub fn on_rx(&mut self, data: &[u8], now_us: i64) -> Option<DhcpEvent> {
        if data.len() < 240 || data[0] != 2 || data[236..240] != COOKIE {
            return None;
        }
        let xid = u32::from_be_bytes(data[4..8].try_into().unwrap());
        if xid != self.xid || data[28..34] != self.mac {
            return None;
        }
        let yiaddr = Ipv4Addr::new(data[16], data[17], data[18], data[19]);

        let mut msg_type = None;
        let mut server_id = None;
        let mut netmask = None;
        let mut router = None;
        let mut lease_secs = DEFAULT_LEASE_SECS;

        let opts = &data[240..];
        let mut i = 0;
        while i < opts.len() {
            let code = opts[i];
            if code == 255 {
                break;
            }
            if code == 0 {
                i += 1;
                continue;
            }
            if i + 1 >= opts.len() {
                break;
            }
            let len = opts[i + 1] as usize;
            if i + 2 + len > opts.len() {
                break;
            }
            let val = &opts[i + 2..i + 2 + len];
            match code {
                53 if len == 1 => msg_type = Some(val[0]),
                54 if len == 4 => server_id = Some(Ipv4Addr::new(val[0], val[1], val[2], val[3])),
                1 if len == 4 => netmask = Some(Ipv4Addr::new(val[0], val[1], val[2], val[3])),
                3 if len >= 4 => router = Some(Ipv4Addr::new(val[0], val[1], val[2], val[3])),
                51 if len == 4 => {
                    lease_secs = u32::from_be_bytes(val.try_into().unwrap()).max(10);
                }
                _ => {}
            }
            i += 2 + len;
        }

        let msg_type = msg_type?;
        match (self.state, msg_type) {
            (State::Discovering, MSG_OFFER) => {
                // Ignore ProxyDHCP offers (yiaddr == 0.0.0.0); only select offers with a real IP.
                if yiaddr.is_unspecified() {
                    return None;
                }
                let server_id = server_id?;
                let offered = DhcpConfig {
                    ip: yiaddr,
                    netmask: netmask.unwrap_or(Ipv4Addr::new(255, 255, 255, 0)),
                    router,
                    server_id,
                    lease_secs,
                };
                self.state = State::Requesting { offered };
                self.next_tx_us = now_us;
                self.retries = 0;
                None
            }
            (State::Requesting { .. } | State::Renewing { .. }, MSG_ACK) => {
                if yiaddr.is_unspecified() {
                    return None;
                }
                let base = if let State::Requesting { offered } = self.state {
                    offered
                } else {
                    self.config?
                };
                let cfg = DhcpConfig {
                    ip: yiaddr,
                    netmask: netmask.unwrap_or(base.netmask),
                    router: router.or(base.router),
                    server_id: server_id.unwrap_or(base.server_id),
                    lease_secs,
                };
                self.enter_bound(cfg, now_us)
            }
            (State::Requesting { .. } | State::Renewing { .. }, MSG_NAK) => {
                let was_bound = self.config.take().is_some();
                self.state = State::Discovering;
                self.next_tx_us = now_us;
                self.retries = 0;
                was_bound.then_some(DhcpEvent::Deconfigured)
            }
            _ => None,
        }
    }

    fn enter_bound(&mut self, cfg: DhcpConfig, now_us: i64) -> Option<DhcpEvent> {
        let changed = self.config != Some(cfg);
        self.config = Some(cfg);
        let lease_us = (cfg.lease_secs as i64) * 1_000_000;
        self.state = State::Bound {
            t1_us: now_us + lease_us / 2,
            t2_us: now_us + (lease_us * 7) / 8,
            expire_us: now_us + lease_us,
        };
        self.retries = 0;
        changed.then_some(DhcpEvent::Bound(cfg))
    }
}
