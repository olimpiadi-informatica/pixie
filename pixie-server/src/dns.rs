//! Built-in DNS server and optional upstream forwarder using `hickory-resolver`.

use crate::{listener_ips, state::State};
use anyhow::Result;
use futures::FutureExt;
use hickory_resolver::{
    TokioResolver,
    config::ResolveHosts,
    net::runtime::TokioRuntimeProvider,
    proto::{
        op::{Message, MessageType, OpCode, Query, ResponseCode},
        rr::RecordType,
    },
    system_conf::read_system_conf,
};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};
use tokio::net::UdpSocket;

const DNS_PORT: u16 = 53;

pub(crate) fn build_upstream_resolver(local_ips: &[Ipv4Addr]) -> Option<Arc<TokioResolver>> {
    let (mut config, mut opts) = read_system_conf().ok()?;
    config.name_servers.retain(|ns| {
        !ns.ip.is_unspecified() && !matches!(ns.ip, IpAddr::V4(v4) if local_ips.contains(&v4))
    });
    if config.name_servers.is_empty() {
        return None;
    }
    opts.use_hosts_file = ResolveHosts::Never;
    let mut builder = TokioResolver::builder_with_config(config, TokioRuntimeProvider::default());
    *builder.options_mut() = opts;
    builder.build().ok().map(Arc::new)
}

fn make_response_base(req: &Message, query: &Query, authoritative: bool) -> Message {
    let mut reply = Message::response(req.id, OpCode::Query);
    reply.metadata.recursion_desired = req.recursion_desired;
    reply.metadata.recursion_available = true;
    reply.metadata.authoritative = authoritative;
    reply.add_query(query.clone());
    reply
}

async fn send_reply(socket: &UdpSocket, peer: SocketAddr, reply: &Message) {
    if let Ok(bytes) = reply.to_vec() {
        let _ = socket.send_to(&bytes, peer).await;
    }
}

async fn serve_dns_ip(state: Arc<State>, local_ip: Ipv4Addr) -> Result<()> {
    let socket = match UdpSocket::bind((local_ip, DNS_PORT)).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            log::warn!(
                "Could not bind DNS server on {local_ip}:{DNS_PORT} ({e}); continuing without DNS listener on {local_ip}"
            );
            state.cancel_token.cancelled().await;
            return Ok(());
        }
    };
    log::info!("DNS listening on {}", socket.local_addr()?);

    let mut config_rx = state.subscribe_config();
    let mut upstream = build_upstream_resolver(&listener_ips(&config_rx.borrow_and_update()));
    let mut dns_hosts_rx = state.subscribe_dns_hosts();
    let mut hosts = dns_hosts_rx.borrow_and_update().clone();
    let mut buf = [0u8; 4096];
    loop {
        let (len, peer) = tokio::select! {
            res = socket.recv_from(&mut buf) => res?,
            _ = state.cancel_token.cancelled() => break,
        };

        if config_rx.has_changed().unwrap_or(false) {
            upstream = build_upstream_resolver(&listener_ips(&config_rx.borrow_and_update()));
        }
        if dns_hosts_rx.has_changed().unwrap_or(false) {
            hosts = dns_hosts_rx.borrow_and_update().clone();
        }

        let Ok(req) = Message::from_vec(&buf[..len]) else {
            continue;
        };
        if req.message_type != MessageType::Query || req.op_code != OpCode::Query {
            continue;
        }
        let Some(query) = req.queries.first().cloned() else {
            continue;
        };

        if let Some(lookup) = hosts.lookup_static_host(&query) {
            let mut reply = make_response_base(&req, &query, true);
            reply.add_answers(lookup.answers().iter().cloned());
            send_reply(&socket, peer, &reply).await;
        } else if hosts
            .lookup_static_host(&Query::query(query.name().clone(), RecordType::A))
            .is_some()
            || hosts
                .lookup_static_host(&Query::query(query.name().clone(), RecordType::AAAA))
                .is_some()
        {
            // Local hostname exists in hostsfile, but has no record for the requested type (e.g. AAAA).
            let reply = make_response_base(&req, &query, true);
            send_reply(&socket, peer, &reply).await;
        } else if state.config().hosts.dns_upstream {
            let mut reply = make_response_base(&req, &query, false);
            if let Some(upstream) = upstream.clone() {
                let socket = socket.clone();
                tokio::spawn(async move {
                    match upstream
                        .lookup(query.name().clone(), query.query_type())
                        .await
                    {
                        Ok(lookup) => {
                            reply.add_answers(lookup.answers().iter().cloned());
                        }
                        Err(e) if e.is_nx_domain() => {
                            reply.metadata.response_code = ResponseCode::NXDomain;
                        }
                        Err(e) if e.is_no_records_found() => {}
                        Err(_) => {
                            reply.metadata.response_code = ResponseCode::ServFail;
                        }
                    }
                    send_reply(&socket, peer, &reply).await;
                });
            } else {
                reply.metadata.response_code = ResponseCode::ServFail;
                send_reply(&socket, peer, &reply).await;
            }
        } else {
            let mut reply = make_response_base(&req, &query, true);
            reply.metadata.response_code = ResponseCode::NXDomain;
            send_reply(&socket, peer, &reply).await;
        }
    }

    Ok(())
}

pub async fn main(state: Arc<State>) -> Result<()> {
    let ips = listener_ips(&state.config());
    let tasks = ips
        .into_iter()
        .map(|ip| serve_dns_ip(state.clone(), ip).boxed())
        .collect::<Vec<_>>();
    futures::future::try_join_all(tasks).await?;
    Ok(())
}
