use std::{
    collections::BTreeMap,
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
    web::{Bytes, Data, Json, Path, PayloadConfig},
    App, HttpRequest, HttpServer, Responder,
};
use anyhow::{anyhow, Context, Result};
use mktemp::Temp;
use serde::Deserialize;

use pixie_shared::{Station, StationKind};

use crate::{find_interface_ip, find_mac, State, Unit};

#[derive(Clone, Debug, Deserialize, Copy)]
pub struct Config {
    pub max_payload: usize,
    pub listen_on: SocketAddrV4,
}

#[derive(Debug, Deserialize, Clone)]
pub struct BootConfig {
    pub unregistered: String,
    pub default: String,
    pub modes: BTreeMap<String, String>,
}

#[get("/action/{mac}/{value}")]
async fn action(
    path: Path<(String, String)>,
    state: Data<State>,
) -> Result<impl Responder, Box<dyn Error>> {
    let units = &mut **state
        .units
        .write()
        .map_err(|_| anyhow!("units mutex is poisoned"))?;

    let value = &path.1;
    if state.config.boot.modes.get(value).is_none() {
        return Ok(format!("Unknown action {}", value)
            .customize()
            .with_status(StatusCode::BAD_REQUEST));
    }

    let mut updated = 0usize;

    if let Ok(mac) = path.0.parse() {
        for unit in units.iter_mut() {
            if unit.mac == mac {
                unit.action = value.clone();
                updated += 1;
            }
        }
    } else if let Ok(ip) = path.0.parse::<Ipv4Addr>() {
        for unit in units.iter_mut() {
            if unit.ip() == ip {
                unit.action = value.clone();
                updated += 1;
            }
        }
    } else if path.0 == "all" {
        for unit in units.iter_mut() {
            unit.action = value.clone();
            updated += 1;
        }
    } else if let Some(&group) = state.config.groups.get(&path.0) {
        for unit in units.iter_mut() {
            if unit.group == group {
                unit.action = value.clone();
                updated += 1;
            }
        }
    } else {
        return Ok("Unknown PC"
            .to_owned()
            .customize()
            .with_status(StatusCode::BAD_REQUEST));
    }

    fs::write(&state.registered_file(), serde_json::to_string(units)?)?;
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
            let mut guard = state
                .hint
                .lock()
                .map_err(|_| anyhow!("hint mutex is poisoned"))?;
            *guard = Station {
                kind: data.kind,
                row: data.row,
                col: data.col + 1,
                group: data.group,
            };

            let IpAddr::V4(peer_ip) = req.peer_addr().unwrap().ip() else {
                Err(anyhow!("IPv6 is not supported"))?
            };
            let peer_mac = find_mac(peer_ip)?;

            let units = &mut *state
                .units
                .write()
                .map_err(|_| anyhow!("units mutex is poisoned"))?;

            let unit = units
                .iter_mut()
                .position(|x| x.mac == peer_mac)
                .map(|unit| {
                    units[unit].kind = data.kind;
                    units[unit].group = data.group;
                    units[unit].row = data.row;
                    units[unit].col = data.col;
                    unit
                })
                .unwrap_or_else(|| {
                    units.push(Unit {
                        mac: peer_mac,
                        kind: data.kind,
                        group: data.group,
                        row: data.row,
                        col: data.col,
                        action: state.config.boot.default.clone(),
                    });
                    units.len() - 1
                });

            let ip = units[unit].ip();

            let mut dnsmasq_lock = state
                .dnsmasq_handle
                .lock()
                .map_err(|_| anyhow!("dnsmasq_handle mutex is poisoned"))?;
            dnsmasq_lock
                .write_host(unit, peer_mac, ip)
                .context("writing hosts file")?;
            dnsmasq_lock.send_sighup().context("sending sighup")?;

            fs::write(&state.registered_file(), serde_json::to_string(&units)?)?;
            return Ok("".customize());
        }
    }

    Ok("Invalid payload"
        .customize()
        .with_status(StatusCode::BAD_REQUEST))
}

#[get("/register_hint")]
async fn register_hint(state: Data<State>) -> Result<impl Responder, Box<dyn Error>> {
    let data = *state
        .hint
        .lock()
        .map_err(|_| anyhow!("Mutex is poisoned"))?;
    Ok(Json(data))
}

#[get("/has_chunk/{hash}")]
async fn has_chunk(hash: Path<String>, state: Data<State>) -> impl Responder {
    let path = state.storage_dir.join("chunks").join(&*hash);
    if path.exists() {
        "pass"
    } else {
        "send"
    }
}

#[post("/chunk/{hash}")]
async fn upload_chunk(
    body: Bytes,
    hash: Path<String>,
    state: Data<State>,
) -> io::Result<impl Responder> {
    let path = state.storage_dir.join("chunks").join(&*hash);
    let tmp_file = Temp::new_file_in(state.storage_dir.join("tmp"))
        .expect("failed to create tmp file")
        .release();
    let body = body.to_vec();
    fs::write(&tmp_file, body).unwrap();
    fs::rename(&tmp_file, path).unwrap();
    Ok("".customize())
}

#[post("/image/{name}")]
async fn upload_image(
    name: Path<String>,
    body: Bytes,
    state: Data<State>,
) -> io::Result<impl Responder> {
    let body = body.to_vec();
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
            .service(has_chunk)
            .service(upload_chunk)
            .service(upload_image)
            .service(Files::new("/image", &images))
            .service(get_chunk)
            .service(register)
            .service(register_hint)
            .service(action)
    })
    .bind(listen_on)?
    .run()
    .await?;

    Ok(())
}
