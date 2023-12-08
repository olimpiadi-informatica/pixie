use std::{
    io::ErrorKind,
    net::SocketAddr,
    net::{IpAddr, Ipv4Addr},
    path::Path,
    sync::Arc,
};

use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

use anyhow::{anyhow, bail, Context, Result};
use macaddr::MacAddr6;
use mktemp::Temp;

use pixie_shared::{to_hex, Action, ActionKind, TcpRequest, Unit, ACTION_PORT};

use crate::{find_mac, State};

async fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    // TODO(virv): find a better way to make a temporary file
    let tmp = Temp::new_file_in(path.parent().unwrap())?.release();
    fs::write(&tmp, data).await?;
    fs::rename(&tmp, path).await?;
    Ok(())
}

async fn handle_request(state: &State, req: TcpRequest, peer_mac: MacAddr6) -> Result<Vec<u8>> {
    Ok(match req {
        TcpRequest::GetChunkSize(hash) => {
            let path = state.storage_dir.join("chunks").join(to_hex(&hash));
            let csize = match fs::metadata(path).await {
                Ok(meta) => Some(meta.len()),
                Err(e) if e.kind() == ErrorKind::NotFound => None,
                Err(e) => Err(e)?,
            };
            postcard::to_allocvec(&csize)?
        }
        TcpRequest::GetImage(name) => {
            let path = state.storage_dir.join("images").join(name);
            fs::read(path).await?
        }
        TcpRequest::Register(station) => {
            if !state.config.images.contains(&station.image) {
                return Ok(format!("Unknown image: {}", station.image).into_bytes());
            }
            let Some(&group) = state.config.groups.get_by_first(&station.group) else {
                return Ok(format!("Unknown group: {}", station.group).into_bytes());
            };

            let buf;
            {
                let mut guard = state
                    .last
                    .lock()
                    .map_err(|_| anyhow!("last mutex is poisoned"))?;
                *guard = station.clone();

                let mut units = state.units.lock().unwrap();
                let unit = units.iter_mut().position(|unit| unit.mac == peer_mac);
                match unit {
                    Some(unit) => {
                        units[unit].group = group;
                        units[unit].row = station.row;
                        units[unit].col = station.col;
                        units[unit].image = station.image;
                    }
                    None => {
                        let unit = Unit {
                            mac: peer_mac,
                            group,
                            row: station.row,
                            col: station.col,
                            curr_action: None,
                            curr_progress: None,
                            next_action: state.config.boot.default,
                            image: station.image,
                            last_ping_timestamp: 0,
                            last_ping_msg: Vec::new(),
                        };
                        units.push(unit);
                    }
                }

                state
                    .dnsmasq_handle
                    .lock()
                    .expect("dnsmasq mutex is poisoned")
                    .set_hosts(&units)
                    .context("changing dnsmasq hosts")?;

                buf = postcard::to_allocvec(&*units)?;
            }

            atomic_write(&state.registered_file(), &buf).await?;

            Vec::new()
        }
        TcpRequest::UploadChunk(hash, data) => {
            let chunks_path = state.storage_dir.join("chunks");
            let path = chunks_path.join(to_hex(&hash));
            atomic_write(&path, &data).await?;
            Vec::new()
        }
        TcpRequest::UploadImage(name, image) => {
            let path = state.storage_dir.join("images").join(name);
            let data = postcard::to_allocvec(&image)?;
            atomic_write(&path, &data).await?;
            Vec::new()
        }
        TcpRequest::GetAction => {
            let mut units = state.units.lock().unwrap();
            let mut unit = units.iter_mut().find(|unit| unit.mac == peer_mac);
            let action_kind = match unit {
                Some(Unit {
                    curr_action: Some(action),
                    ..
                }) => *action,
                Some(ref mut unit) => {
                    let action = unit.next_action;
                    unit.curr_action = Some(action);
                    if matches!(
                        unit.next_action,
                        ActionKind::Push | ActionKind::Pull | ActionKind::Register
                    ) {
                        unit.next_action = ActionKind::Wait;
                    }
                    action
                }
                None => state.config.boot.unregistered,
            };

            let action = match action_kind {
                ActionKind::Reboot => Action::Reboot,
                ActionKind::Register => Action::Register {
                    hint_port: state.config.hosts.hint_port,
                },
                ActionKind::Push => Action::Push {
                    image: unit.unwrap().image.clone(),
                },
                ActionKind::Pull => Action::Pull {
                    image: unit.unwrap().image.clone(),
                    chunks_port: state.config.hosts.chunks_port,
                },
                ActionKind::Wait => Action::Wait,
            };

            // TODO(virv): async
            std::fs::write(state.registered_file(), serde_json::to_vec(&*units)?)?;
            postcard::to_allocvec(&action)?
        }
        TcpRequest::ActionComplete => {
            let mut units = state.units.lock().unwrap();
            if let Some(unit) = units.iter_mut().find(|unit| unit.mac == peer_mac) {
                unit.curr_action = None;
                unit.curr_progress = None;
                std::fs::write(state.registered_file(), serde_json::to_vec(&*units)?)?;
            } else {
                log::warn!("Got NA from unknown unit");
            };

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
    let peer_mac = find_mac(peer_ip)?;

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
