use std::{net::IpAddr, sync::Arc, time::SystemTime};

use anyhow::{bail, Result};
use tokio::net::UdpSocket;

use crate::{find_mac, state::State};
use pixie_shared::{PACKET_LEN, PING_PORT};

pub async fn main(state: Arc<State>) -> Result<()> {
    let socket = UdpSocket::bind((state.config.hosts.listen_on, PING_PORT)).await?;
    log::info!("Listening on {}", socket.local_addr()?);

    let mut buf = [0; PACKET_LEN];
    loop {
        let (len, peer_addr) = socket.recv_from(&mut buf).await?;
        let IpAddr::V4(peer_ip) = peer_addr.ip() else {
            bail!("IPv6 is not supported")
        };
        let peer_mac = match find_mac(peer_ip) {
            Ok(peer_mac) => peer_mac,
            Err(err) => {
                log::error!("Error handling ping packet: {}", err);
                continue;
            }
        };

        let time = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        state.set_ping(peer_mac, time, buf[..len].to_owned());
    }
}
