//! Built-in DNS server and optional upstream forwarder using `hickory-resolver`.

use crate::state::State;
use anyhow::Result;
use hickory_resolver::{
    Hosts, TokioResolver,
    config::ResolveHosts,
    lookup::Lookup,
    net::runtime::TokioRuntimeProvider,
    proto::{
        op::{Message, MessageType, Query, ResponseCode},
        rr::{Name, RData, RecordType},
    },
    system_conf::read_system_conf,
};
use std::{collections::HashMap, net::Ipv4Addr, str::FromStr, sync::Arc};
use tokio::net::UdpSocket;

const DNS_PORT: u16 = 53;

fn build_hosts(hostmap: &HashMap<Ipv4Addr, String>) -> Hosts {
    let mut hosts = Hosts::default();
    for (&ip, hostname) in hostmap {
        if let Ok(name) = Name::from_str(hostname) {
            let query = Query::query(name.clone(), RecordType::A);
            hosts.insert(
                name,
                RecordType::A,
                Lookup::from_rdata(query, RData::A(ip.into())),
            );
        }
    }
    hosts
}

fn build_upstream_resolver() -> Option<TokioResolver> {
    let (mut config, mut opts) = read_system_conf().ok()?;
    config
        .name_servers
        .retain(|ns| !ns.ip.is_loopback() && !ns.ip.is_unspecified());
    if config.name_servers.is_empty() {
        return None;
    }
    opts.use_hosts_file = ResolveHosts::Never;
    let mut builder = TokioResolver::builder_with_config(config, TokioRuntimeProvider::default());
    *builder.options_mut() = opts;
    builder.build().ok()
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

    let upstream = build_upstream_resolver();
    let hosts = build_hosts(&state.hostmap);
    let mut buf = [0u8; 4096];
    loop {
        let (len, peer) = tokio::select! {
            res = socket.recv_from(&mut buf) => res?,
            _ = state.cancel_token.cancelled() => break,
        };

        let Ok(req) = Message::from_vec(&buf[..len]) else {
            continue;
        };
        if req.message_type != MessageType::Query {
            continue;
        }
        let Some(query) = req.queries.first().cloned() else {
            continue;
        };

        let mut reply = req.into_response();
        reply.metadata.authoritative = true;
        reply.metadata.recursion_available = true;

        if let Some(lookup) = hosts.lookup_static_host(&query) {
            reply.add_answers(lookup.answers().iter().cloned());
        } else if hosts
            .lookup_static_host(&Query::query(query.name().clone(), RecordType::A))
            .is_some()
        {
            // Local hostname exists in hostsfile, but has no record for the requested type (e.g. AAAA).
        } else if state.config.hosts.dns_upstream {
            reply.metadata.authoritative = false;
            match upstream
                .as_ref()
                .map(|r| r.lookup(query.name().clone(), query.query_type()))
            {
                Some(fut) => match fut.await {
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
                },
                None => {
                    reply.metadata.response_code = ResponseCode::ServFail;
                }
            }
        } else {
            reply.metadata.response_code = ResponseCode::NXDomain;
        }

        if let Ok(bytes) = reply.to_vec() {
            let _ = socket.send_to(&bytes, peer).await;
        }
    }

    Ok(())
}
