//! Built-in read-only TFTP server using the `tftpd` crate.

use crate::state::State;
use anyhow::{Result, anyhow};
use std::{
    net::{IpAddr, Ipv4Addr},
    sync::{Arc, atomic::Ordering},
};
use tftpd::{Config, Server};

const TFTP_PORT: u16 = 69;

pub async fn main(state: Arc<State>) -> Result<()> {
    let tftp_dir = state.storage_dir.join("tftpboot");
    let config = Config {
        ip_address: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        port: TFTP_PORT,
        directory: tftp_dir.clone(),
        send_directory: tftp_dir.clone(),
        receive_directory: tftp_dir,
        read_only: true,
        ..Default::default()
    };
    let mut server =
        Server::new(&config).map_err(|e| anyhow!("Failed to start TFTP server: {e}"))?;
    log::info!("TFTP listening on 0.0.0.0:{TFTP_PORT}");

    let abort_flag = server.get_abort_flag();
    let handle = tokio::task::spawn_blocking(move || server.listen());

    state.cancel_token.cancelled().await;
    abort_flag.store(true, Ordering::Relaxed);
    handle.await?;
    Ok(())
}
