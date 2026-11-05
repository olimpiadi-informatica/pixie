//! Built-in DNS server and optional upstream forwarder using `simple-dns`.

use crate::state::State;
use anyhow::Result;
use simple_dns::{
    CLASS, Name, Packet, PacketFlag, QCLASS, QTYPE, RCODE, ResourceRecord, TYPE,
    rdata::{A, PTR, RData},
};
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{net::UdpSocket, time::timeout};

const DNS_PORT: u16 = 53;
const LOCAL_TTL: u32 = 300;
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(2);

pub struct LocalDnsTable {
    forward: HashMap<String, Vec<Ipv4Addr>>,
    reverse: HashMap<String, String>,
}

impl LocalDnsTable {
    pub fn from_hostmap(hostmap: &HashMap<Ipv4Addr, String>) -> Self {
        let mut forward: HashMap<String, Vec<Ipv4Addr>> = HashMap::new();
        let mut reverse: HashMap<String, String> = HashMap::new();

        for (&ip, hostname) in hostmap {
            let key = hostname.trim_end_matches('.').to_ascii_lowercase();
            forward.entry(key).or_default().push(ip);

            let octets = ip.octets();
            let ptr_name = format!(
                "{}.{}.{}.{}.in-addr.arpa",
                octets[3], octets[2], octets[1], octets[0]
            );
            reverse.insert(ptr_name, hostname.trim_end_matches('.').to_string());
        }

        Self { forward, reverse }
    }
}

fn read_upstream_nameservers() -> Vec<SocketAddr> {
    let Ok(contents) = std::fs::read_to_string("/etc/resolv.conf") else {
        return Vec::new();
    };
    let mut servers = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("nameserver") {
            let addr_str = rest.split_whitespace().next().unwrap_or("");
            if let Ok(ip) = addr_str.parse::<IpAddr>()
                && !ip.is_loopback()
                && !ip.is_unspecified()
            {
                servers.push(SocketAddr::new(ip, DNS_PORT));
            }
        }
    }
    servers
}

/// Attempts to answer `query_bytes` from `table`.
/// Returns `Ok(Some(response_bytes))` if answered locally, `Ok(None)` if it should be forwarded upstream,
/// or `Err` if the packet is malformed.
pub fn answer_local_query(
    table: &LocalDnsTable,
    query_bytes: &[u8],
    allow_upstream: bool,
) -> Result<Option<Vec<u8>>> {
    let query = Packet::parse(query_bytes)?;
    if query.has_flags(PacketFlag::RESPONSE) || query.questions.is_empty() {
        return Ok(None);
    }

    let rd = query.has_flags(PacketFlag::RECURSION_DESIRED);
    let mut reply = Packet::new_reply(query.id());
    let mut flags =
        PacketFlag::RESPONSE | PacketFlag::AUTHORITATIVE_ANSWER | PacketFlag::RECURSION_AVAILABLE;
    if rd {
        flags |= PacketFlag::RECURSION_DESIRED;
    }
    reply.set_flags(flags);

    let mut matched_local_name = false;

    for q in &query.questions {
        reply.questions.push(q.clone());
        if !matches!(q.qclass, QCLASS::CLASS(CLASS::IN) | QCLASS::ANY) {
            continue;
        }

        let qname = q.qname.to_string();
        let qname_norm = qname.trim_end_matches('.').to_ascii_lowercase();

        if let Some(ips) = table.forward.get(&qname_norm) {
            matched_local_name = true;
            if matches!(q.qtype, QTYPE::TYPE(TYPE::A) | QTYPE::ANY) {
                for &ip in ips {
                    reply.answers.push(ResourceRecord::new(
                        q.qname.clone(),
                        CLASS::IN,
                        LOCAL_TTL,
                        RData::A(A::from(ip)),
                    ));
                }
            }
        }

        if let Some(target_host) = table.reverse.get(&qname_norm) {
            matched_local_name = true;
            if matches!(q.qtype, QTYPE::TYPE(TYPE::PTR) | QTYPE::ANY)
                && let Ok(name) = Name::new(target_host)
            {
                reply.answers.push(ResourceRecord::new(
                    q.qname.clone(),
                    CLASS::IN,
                    LOCAL_TTL,
                    RData::PTR(PTR(name.into_owned())),
                ));
            }
        }
    }

    if matched_local_name {
        return Ok(Some(reply.build_bytes_vec_compressed()?));
    }

    if allow_upstream {
        return Ok(None);
    }

    *reply.rcode_mut() = RCODE::NameError;
    Ok(Some(reply.build_bytes_vec_compressed()?))
}

async fn forward_to_upstream(query_bytes: &[u8], upstreams: &[SocketAddr]) -> Option<Vec<u8>> {
    for &upstream in upstreams {
        let bind_addr: SocketAddr = if upstream.is_ipv4() {
            "0.0.0.0:0".parse().unwrap()
        } else {
            "[::]:0".parse().unwrap()
        };
        let Ok(sock) = UdpSocket::bind(bind_addr).await else {
            continue;
        };
        if sock.send_to(query_bytes, upstream).await.is_err() {
            continue;
        }
        let mut buf = [0u8; 4096];
        if let Ok(Ok((len, _))) = timeout(UPSTREAM_TIMEOUT, sock.recv_from(&mut buf)).await {
            return Some(buf[..len].to_vec());
        }
    }
    None
}

pub async fn main(state: Arc<State>) -> Result<()> {
    let socket = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, DNS_PORT)).await {
        Ok(s) => s,
        Err(e) => {
            log::warn!(
                "Could not bind DNS server on 0.0.0.0:{DNS_PORT} ({e}); continuing without DNS listener"
            );
            state.cancel_token.cancelled().await;
            return Ok(());
        }
    };
    log::info!("DNS listening on {}", socket.local_addr()?);

    let mut hostmap_rx = state.subscribe_hostmap();
    let mut config_rx = state.subscribe_config();

    let mut table = LocalDnsTable::from_hostmap(&hostmap_rx.borrow_and_update());
    let mut upstreams = if config_rx.borrow_and_update().hosts.dns_upstream {
        read_upstream_nameservers()
    } else {
        Vec::new()
    };

    let mut buf = [0u8; 4096];
    loop {
        let (len, peer) = tokio::select! {
            res = socket.recv_from(&mut buf) => res?,
            Ok(()) = hostmap_rx.changed() => {
                table = LocalDnsTable::from_hostmap(&hostmap_rx.borrow_and_update());
                log::info!("Reloaded DNS hostmap ({} entries)", table.forward.len());
                continue;
            }
            Ok(()) = config_rx.changed() => {
                let dns_upstream = config_rx.borrow_and_update().hosts.dns_upstream;
                upstreams = if dns_upstream {
                    read_upstream_nameservers()
                } else {
                    Vec::new()
                };
                log::info!("Reloaded DNS config (dns_upstream={dns_upstream})");
                continue;
            }
            _ = state.cancel_token.cancelled() => break,
        };

        let query_pkt = buf[..len].to_vec();
        let allow_upstream = !upstreams.is_empty();
        match answer_local_query(&table, &query_pkt, allow_upstream) {
            Ok(Some(reply_bytes)) => {
                let _ = socket.send_to(&reply_bytes, peer).await;
            }
            Ok(None) => {
                let query_id = Packet::parse(&query_pkt).map(|p| p.id()).ok();
                let resp = forward_to_upstream(&query_pkt, &upstreams).await;
                if let Some(resp_bytes) = resp {
                    let _ = socket.send_to(&resp_bytes, peer).await;
                } else if let Some(id) = query_id {
                    let mut fail = Packet::new_reply(id);
                    *fail.rcode_mut() = RCODE::ServerFailure;
                    if let Ok(bytes) = fail.build_bytes_vec_compressed() {
                        let _ = socket.send_to(&bytes, peer).await;
                    }
                }
            }
            Err(_) => {}
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use simple_dns::Question;

    #[test]
    fn test_local_dns_a_and_ptr_queries() {
        let mut hostmap = HashMap::new();
        hostmap.insert(
            Ipv4Addr::new(10, 0, 1, 1),
            "pc-01.contest.local".to_string(),
        );
        let table = LocalDnsTable::from_hostmap(&hostmap);

        // 1. A query for pc-01.contest.local
        let mut q = Packet::new_query(0x1234);
        q.questions.push(Question::new(
            Name::new_unchecked("pc-01.contest.local"),
            QTYPE::TYPE(TYPE::A),
            QCLASS::CLASS(CLASS::IN),
            false,
        ));
        let q_bytes = q.build_bytes_vec().unwrap();

        let resp_bytes = answer_local_query(&table, &q_bytes, false)
            .unwrap()
            .expect("Expected local A response");
        let resp = Packet::parse(&resp_bytes).unwrap();
        assert_eq!(resp.id(), 0x1234);
        assert_eq!(resp.rcode(), RCODE::NoError);
        assert_eq!(resp.answers.len(), 1);
        assert_eq!(
            resp.answers[0].rdata,
            RData::A(A::from(Ipv4Addr::new(10, 0, 1, 1)))
        );

        // 2. PTR query for 1.1.0.10.in-addr.arpa
        let mut ptr_q = Packet::new_query(0x5678);
        ptr_q.questions.push(Question::new(
            Name::new_unchecked("1.1.0.10.in-addr.arpa"),
            QTYPE::TYPE(TYPE::PTR),
            QCLASS::CLASS(CLASS::IN),
            false,
        ));
        let ptr_bytes = ptr_q.build_bytes_vec().unwrap();

        let ptr_resp_bytes = answer_local_query(&table, &ptr_bytes, false)
            .unwrap()
            .expect("Expected local PTR response");
        let ptr_resp = Packet::parse(&ptr_resp_bytes).unwrap();
        assert_eq!(ptr_resp.rcode(), RCODE::NoError);
        assert_eq!(ptr_resp.answers.len(), 1);
        assert_eq!(
            ptr_resp.answers[0].rdata,
            RData::PTR(PTR(Name::new_unchecked("pc-01.contest.local")))
        );

        // 3. Unknown domain with allow_upstream = false -> NXDOMAIN
        let mut unk_q = Packet::new_query(0x9999);
        unk_q.questions.push(Question::new(
            Name::new_unchecked("unknown.example.com"),
            QTYPE::TYPE(TYPE::A),
            QCLASS::CLASS(CLASS::IN),
            false,
        ));
        let unk_resp_bytes = answer_local_query(&table, &unk_q.build_bytes_vec().unwrap(), false)
            .unwrap()
            .unwrap();
        let unk_resp = Packet::parse(&unk_resp_bytes).unwrap();
        assert_eq!(unk_resp.rcode(), RCODE::NameError);
    }

    #[tokio::test]
    async fn test_state_reload_updates_config_and_hostmap() {
        use ipnet::Ipv4Net;
        use pixie_shared::{Bijection, Config, DhcpMode, HostsConfig, HttpConfig, InterfaceConfig};

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("chunks")).unwrap();
        std::fs::create_dir_all(dir.path().join("images")).unwrap();
        std::fs::create_dir_all(dir.path().join("tftpboot")).unwrap();

        let hosts_path = dir.path().join("hosts");
        std::fs::write(&hosts_path, "10.0.1.1 pc-01.contest.local\n").unwrap();

        let groups: Bijection<String, u8> = serde_yaml::from_str("- [room0, 0]\n").unwrap();
        let mut config = Config {
            hosts: HostsConfig {
                interfaces: vec![InterfaceConfig {
                    network: "10.0.0.1/16".parse::<Ipv4Net>().unwrap(),
                    dhcp: DhcpMode::Proxy(Ipv4Addr::new(10, 0, 0, 1)),
                    broadcast_speed: 50_000_000,
                }],
                hostsfile: Some(hosts_path.clone()),
                dns_upstream: true,
            },
            http: HttpConfig {
                listen_on: "127.0.0.1:0".to_string(),
            },
            groups,
            images: vec!["contestant".to_string()],
        };

        std::fs::write(
            dir.path().join("config.yaml"),
            serde_yaml::to_string(&config).unwrap(),
        )
        .unwrap();

        let state = State::load(dir.path().to_path_buf()).unwrap();
        assert!(state.config().hosts.dns_upstream);
        assert_eq!(
            state
                .hostmap()
                .get(&Ipv4Addr::new(10, 0, 1, 1))
                .map(String::as_str),
            Some("pc-01.contest.local")
        );

        // Disable dns_upstream and add a new host to hostsfile, then reload
        config.hosts.dns_upstream = false;
        std::fs::write(
            dir.path().join("config.yaml"),
            serde_yaml::to_string(&config).unwrap(),
        )
        .unwrap();
        std::fs::write(
            &hosts_path,
            "10.0.1.1 pc-01.contest.local\n10.0.1.2 pc-02.contest.local\n",
        )
        .unwrap();

        state.reload().unwrap();
        assert!(!state.config().hosts.dns_upstream);
        assert_eq!(
            state
                .hostmap()
                .get(&Ipv4Addr::new(10, 0, 1, 2))
                .map(String::as_str),
            Some("pc-02.contest.local")
        );
    }
}
