use crate::{
    find_mac,
    state::{State, UnitSelector},
};
use anyhow::{bail, Context, Result};
use macaddr::MacAddr6;
use pixie_shared::{TcpRequest, ACTION_PORT};
use std::{
    io::ErrorKind,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

async fn handle_request(state: &State, req: TcpRequest, peer_mac: MacAddr6) -> Result<Vec<u8>> {
    Ok(match req {
        TcpRequest::HasChunk(hash) => {
            let has_chunk = state.has_chunk(hash);
            postcard::to_allocvec(&has_chunk)?
        }
        TcpRequest::GetImage => {
            let unit = state.get_unit(peer_mac).context("Unit not found")?;
            state.get_image_serialized(&unit.image)?.unwrap()
        }
        TcpRequest::Register(station) => {
            state.set_last(station.clone());
            state.register_unit(peer_mac, station)?;
            Vec::new()
        }
        TcpRequest::UploadChunk(data) => {
            state.add_chunk(&data)?;
            Vec::new()
        }
        TcpRequest::UploadImage(image) => {
            let unit = state.get_unit(peer_mac).context("Unit not found")?;
            state.add_image(unit.image, &image)?;
            Vec::new()
        }
        TcpRequest::GetAction => {
            let action = state.get_unit_action(peer_mac);
            postcard::to_allocvec(&action)?
        }
        TcpRequest::ActionComplete => {
            state.unit_complete_action(UnitSelector::MacAddr(peer_mac));
            Vec::new()
        }
    })
}

async fn handle_connection(
    state: Arc<State>,
    mut stream: TcpStream,
    peer_addr: SocketAddr,
) -> Result<()> {
    let IpAddr::V4(peer_ip) = peer_addr.ip() else {
        bail!("IPv6 is not supported")
    };
    let peer_mac = match find_mac(peer_ip) {
        Ok(peer_mac) => peer_mac,
        Err(err) => {
            log::error!("Error handling tcp connection: {}", err);
            return Ok(());
        }
    };

    loop {
        let len = match stream.read_u64_le().await {
            Ok(len) => len as usize,
            Err(e) if e.kind() == ErrorKind::ConnectionReset => return Ok(()),
            Err(e) => Err(e)?,
        };
        let mut buf = vec![0; len];
        stream.read_exact(&mut buf).await?;
        let req = postcard::from_bytes(&buf)?;
        let resp = handle_request(&state, req, peer_mac).await?;
        stream.write_u64_le(resp.len() as u64).await?;
        stream.write_all(&resp).await?;
    }
}

pub async fn main(state: Arc<State>) -> Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, ACTION_PORT)).await?;
    log::info!("Listening on {}", listener.local_addr()?);
    loop {
        let (stream, addr) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(state, stream, addr).await {
                log::error!("Error handling tcp connection: {}", e);
            }
        });
    }
}
