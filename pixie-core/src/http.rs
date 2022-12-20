use std::{
    error::Error,
    fs, io,
    net::{IpAddr, Ipv4Addr, SocketAddrV4},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use actix_files::Files;
use actix_web::{
    get,
    http::StatusCode,
    middleware::Logger,
    post,
    web::{Bytes, Data, Path, PayloadConfig},
    App, HttpRequest, HttpServer, Responder,
};
use anyhow::{anyhow, Context, Result};
use mktemp::Temp;
use serde::Deserialize;

use pixie_shared::Station;

use crate::{find_mac, State, Unit};

#[derive(Clone, Debug, Deserialize, Copy)]
pub struct Config {
    pub max_payload: usize,
    pub listen_on: SocketAddrV4,
}

#[get("/action/{mac}/{value}")]
async fn action(
    path: Path<(String, String)>,
    state: Data<State>,
) -> Result<impl Responder, Box<dyn Error>> {
    let units = &mut **state
        .units
        .lock()
        .map_err(|_| anyhow!("units mutex is poisoned"))?;

    let Ok(action) = path.1.parse() else {
        return Ok(format!("Unknown action: {}", path.1)
            .customize()
            .with_status(StatusCode::BAD_REQUEST));
    };

    let mut updated = 0usize;

    if let Ok(mac) = path.0.parse() {
        for unit in units.iter_mut() {
            if unit.mac == mac {
                unit.next_action = action;
                updated += 1;
            }
        }
    } else if let Ok(ip) = path.0.parse::<Ipv4Addr>() {
        for unit in units.iter_mut() {
            if unit.ip() == ip {
                unit.next_action = action;
                updated += 1;
            }
        }
    } else if path.0 == "all" {
        for unit in units.iter_mut() {
            unit.next_action = action;
            updated += 1;
        }
    } else if let Some(&group) = state.config.groups.get(&path.0) {
        for unit in units.iter_mut() {
            if unit.group == group {
                unit.next_action = action;
                updated += 1;
            }
        }
    } else {
        return Ok("Unknown PC"
            .to_owned()
            .customize()
            .with_status(StatusCode::BAD_REQUEST));
    }

    fs::write(state.registered_file(), serde_json::to_string(units)?)?;
    Ok(format!("{updated} computer(s) affected\n").customize())
}

#[post("/register")]
async fn register(
    req: HttpRequest,
    body: Bytes,
    state: Data<State>,
) -> Result<impl Responder, Box<dyn Error>> {
    let body = body.to_vec();
    if let Ok(s) = std::str::from_utf8(&body) {
        if let Ok(data) = serde_json::from_str::<Station>(s) {
            if !state.config.images.contains(&data.image) {
                return Ok(format!("Unknown image: {}", data.image)
                    .customize()
                    .with_status(StatusCode::BAD_REQUEST));
            }

            let mut guard = state
                .last
                .lock()
                .map_err(|_| anyhow!("hint mutex is poisoned"))?;
            *guard = data.clone();

            let IpAddr::V4(peer_ip) = req.peer_addr().unwrap().ip() else {
                Err(anyhow!("IPv6 is not supported"))?
            };
            let peer_mac = find_mac(peer_ip)?;

            let units = &mut *state
                .units
                .lock()
                .map_err(|_| anyhow!("units mutex is poisoned"))?;

            let unit = units.iter_mut().position(|x| x.mac == peer_mac);
            let unit = match unit {
                Some(unit) => {
                    units[unit].group = data.group;
                    units[unit].row = data.row;
                    units[unit].col = data.col;
                    units[unit].image = data.image;
                    unit
                }
                None => {
                    let unit = Unit {
                        mac: peer_mac,
                        group: data.group,
                        row: data.row,
                        col: data.col,
                        image: data.image,
                        curr_action: None,
                        next_action: state.config.boot.default,
                    };
                    units.push(unit);
                    units.len() - 1
                }
            };

            let ip = units[unit].ip();

            let mut dnsmasq_lock = state
                .dnsmasq_handle
                .lock()
                .map_err(|_| anyhow!("dnsmasq_handle mutex is poisoned"))?;
            dnsmasq_lock
                .write_host(unit, peer_mac, ip)
                .context("writing hosts file")?;
            dnsmasq_lock.send_sighup().context("sending sighup")?;

            fs::write(state.registered_file(), serde_json::to_string(&units)?)?;
            return Ok("".to_owned().customize());
        }
    }

    Ok("Invalid payload"
        .to_owned()
        .customize()
        .with_status(StatusCode::BAD_REQUEST))
}

#[get("/get_chunk_csize/{hash}")]
async fn get_chunk_csize(
    hash: Path<String>,
    state: Data<State>,
) -> Result<impl Responder, Box<dyn Error>> {
    let path = state.storage_dir.join("chunks").join(&*hash);
    let csize = match fs::metadata(path) {
        Ok(meta) => Some(meta.len()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => Err(e)?,
    };
    Ok(serde_json::to_vec(&csize)?)
}

#[post("/chunk/{hash}")]
async fn upload_chunk(
    body: Bytes,
    hash: Path<String>,
    state: Data<State>,
) -> io::Result<impl Responder> {
    let chunks_path = state.storage_dir.join("chunks");
    let path = chunks_path.join(&*hash);
    let tmp_file = Temp::new_file_in(chunks_path)
        .expect("failed to create tmp file")
        .release();
    fs::write(&tmp_file, &body)?;
    fs::rename(&tmp_file, &path)?;
    Ok("")
}

#[post("/image/{name}")]
async fn upload_image(
    name: Path<String>,
    body: Bytes,
    state: Data<State>,
) -> io::Result<impl Responder> {
    // TODO(veluca): check the chunks for validity.
    let path = state.storage_dir.join("images").join(&*name);
    fs::write(path, body)?;
    Ok("")
}

#[get("/chunk/{hash}")]
async fn get_chunk(hash: Path<String>, state: Data<State>) -> io::Result<impl Responder> {
    static CONN: AtomicUsize = AtomicUsize::new(0);

    struct Handle;

    impl Handle {
        fn new(limit: usize) -> Option<Self> {
            CONN.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |x| {
                (x < limit).then_some(x + 1)
            })
            .is_ok()
            .then_some(Handle)
        }
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            CONN.fetch_sub(1, Ordering::SeqCst);
        }
    }

    match Handle::new(12) {
        Some(_handle) => Ok(fs::read(state.storage_dir.join("chunks").join(&*hash))?.customize()),
        None => Ok(Vec::new().customize().with_status(StatusCode::IM_A_TEAPOT)),
    }
}

pub async fn main(state: Arc<State>) -> Result<()> {
    let Config {
        max_payload,
        listen_on,
    } = state.config.http;

    let images = state.storage_dir.join("images");
    let data: Data<_> = state.into();

    HttpServer::new(move || {
        App::new()
            .wrap(Logger::default())
            .app_data(PayloadConfig::new(max_payload))
            .app_data(data.clone())
            .service(get_chunk_csize)
            .service(upload_chunk)
            .service(upload_image)
            .service(Files::new("/image", &images))
            .service(get_chunk)
            .service(register)
            .service(action)
    })
    .bind(listen_on)?
    .run()
    .await?;

    Ok(())
}
