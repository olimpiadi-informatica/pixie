//! Handles pings from clients.

use crate::{
    find_mac,
    state::{State, UnitSelector},
};
use anyhow::Result;
use pixie_shared::{PING_PORT, UDP_BODY_LEN};
use std::{net::Ipv4Addr, sync::Arc, time::SystemTime};
use tokio::net::UdpSocket;

pub async fn main(state: Arc<State>) -> Result<()> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, PING_PORT)).await?;
    log::info!("Listening on {}", socket.local_addr()?);

    let mut buf = [0; UDP_BODY_LEN];
    loop {
        let (len, peer_addr) = tokio::select! {
            x = socket.recv_from(&mut buf) => x?,
            _ = state.cancel_token.cancelled() => break,
        };
        let peer_mac = match find_mac(peer_addr.ip()) {
            Ok(peer_mac) => peer_mac,
            Err(err) => {
                log::error!("Error handling ping packet: {err}");
                continue;
            }
        };

        let time = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        state.set_unit_ping(UnitSelector::MacAddr(peer_mac), time, &buf[..len]);
    }
    Ok(())
}
