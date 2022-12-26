use std::{
    error::Error,
    fs, io,
    net::{IpAddr, Ipv4Addr, SocketAddrV4},
    sync::Arc,
};

use actix_files::Files;
use actix_web::{
    error::{ErrorNotImplemented, ErrorUnauthorized},
    get,
    http::StatusCode,
    middleware::Logger,
    post,
    web::{Bytes, Data, Json, Path, PayloadConfig},
    App, HttpRequest, HttpServer, Responder,
};
use actix_web_httpauth::extractors::basic::BasicAuth;
use anyhow::{anyhow, Context, Result};
use macaddr::MacAddr6;
use mktemp::Temp;

use pixie_shared::{HttpConfig, PersistentServerState, Station, Unit};

use crate::{find_mac, State};

#[get("/action/{mac}/{value}")]
async fn action(
    path: Path<(String, String)>,
    state: Data<State>,
) -> Result<impl Responder, Box<dyn Error>> {
    let mut pcfg = state.persistent.lock().unwrap();

    let Ok(action) = path.1.parse() else {
        return Ok(format!("Unknown action: {}", path.1)
            .customize()
            .with_status(StatusCode::BAD_REQUEST));
    };

    let mut updated = 0usize;

    if let Ok(mac) = path.0.parse::<MacAddr6>() {
        for unit in pcfg.units.iter_mut() {
            if unit.mac == mac {
                unit.next_action = action;
                updated += 1;
            }
        }
    } else if let Ok(ip) = path.0.parse::<Ipv4Addr>() {
        for unit in pcfg.units.iter_mut() {
            if unit.static_ip() == ip {
                unit.next_action = action;
                updated += 1;
            }
        }
    } else if path.0 == "all" {
        for unit in pcfg.units.iter_mut() {
            unit.next_action = action;
            updated += 1;
        }
    } else if let Some(&group) = pcfg.config.groups.get(&path.0) {
        for unit in pcfg.units.iter_mut() {
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

    fs::write(state.registered_file(), serde_json::to_string(&pcfg.units)?)?;
    Ok(format!("{updated} computer(s) affected\n").customize())
}

#[post("/register")]
async fn register(
    req: HttpRequest,
    body: Bytes,
    state: Data<State>,
) -> Result<impl Responder, Box<dyn Error>> {
    let mut pcfg = state.persistent.lock().unwrap();
    let body = body.to_vec();
    if let Ok(s) = std::str::from_utf8(&body) {
        if let Ok(data) = serde_json::from_str::<Station>(s) {
            if !pcfg.config.images.contains(&data.image) {
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

            let unit = pcfg.units.iter_mut().position(|x| x.mac == peer_mac);
            match unit {
                Some(unit) => {
                    pcfg.units[unit].group = data.group;
                    pcfg.units[unit].row = data.row;
                    pcfg.units[unit].col = data.col;
                    pcfg.units[unit].image = data.image;
                }
                None => {
                    let unit = Unit {
                        mac: peer_mac,
                        group: data.group,
                        row: data.row,
                        col: data.col,
                        image: data.image,
                        curr_action: None,
                        next_action: pcfg.config.boot.default,
                    };
                    pcfg.units.push(unit);
                }
            };

            state
                .dnsmasq_handle
                .lock()
                .unwrap()
                .set_hosts(&pcfg.units)
                .context("changing dnsmasq hosts")?;

            fs::write(state.registered_file(), serde_json::to_string(&pcfg.units)?)?;
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

#[get("/state")]
async fn get_state(
    auth: BasicAuth,
    state: Data<State>,
) -> Result<impl Responder, actix_web::Error> {
    let config = state.persistent.lock().unwrap();
    if Some(config.config.admin_password.as_str()) != auth.password() {
        return Err(ErrorUnauthorized("password incorrect"));
    }
    Ok(serde_json::to_string(&*config))
}

#[post("/set_state")]
async fn set_state(
    auth: BasicAuth,
    body: Json<PersistentServerState>,
    state: Data<State>,
) -> Result<impl Responder, actix_web::Error> {
    let config = state.persistent.lock().unwrap();
    if Some(config.config.admin_password.as_str()) != auth.password() {
        return Err(ErrorUnauthorized("password incorrect"));
    }
    if *config == body.0 {
        return Ok("");
    }
    return Err(ErrorNotImplemented("not yet implemented"));
}

pub async fn main(state: Arc<State>) -> Result<()> {
    let HttpConfig {
        max_payload,
        listen_on,
    } = state.persistent.lock().unwrap().config.http;

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
            .service(register)
            .service(action)
            .service(get_state)
            .service(set_state)
    })
    .bind(SocketAddrV4::from(listen_on))?
    .run()
    .await?;

    Ok(())
}
