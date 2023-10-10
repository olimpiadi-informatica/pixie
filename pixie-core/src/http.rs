use std::{error::Error, fs, net::Ipv4Addr, sync::Arc};

use actix_files::Files;
use actix_web::{
    error::ErrorUnauthorized,
    get,
    http::StatusCode,
    middleware::Logger,
    web::{Data, Path, PayloadConfig},
    App, HttpServer, Responder,
};
use actix_web_httpauth::middleware::HttpAuthentication;
use anyhow::Result;
use macaddr::MacAddr6;

use pixie_shared::HttpConfig;

use crate::State;

#[get("/admin/action/{mac}/{value}")]
async fn action(
    path: Path<(String, String)>,
    state: Data<State>,
) -> Result<impl Responder, Box<dyn Error>> {
    let mut units = state.units.lock().unwrap();

    let Ok(action) = path.1.parse() else {
        return Ok(format!("Unknown action: {:?}", path.1)
            .customize()
            .with_status(StatusCode::BAD_REQUEST));
    };

    let mut updated = 0usize;

    if let Ok(mac) = path.0.parse::<MacAddr6>() {
        for unit in units.iter_mut() {
            if unit.mac == mac {
                unit.next_action = action;
                updated += 1;
            }
        }
    } else if let Ok(ip) = path.0.parse::<Ipv4Addr>() {
        for unit in units.iter_mut() {
            if unit.static_ip() == ip {
                unit.next_action = action;
                updated += 1;
            }
        }
    } else if path.0 == "all" {
        for unit in units.iter_mut() {
            unit.next_action = action;
            updated += 1;
        }
    } else if let Some(&group) = state.config.groups.get_by_first(&path.0) {
        for unit in units.iter_mut() {
            if unit.group == group {
                unit.next_action = action;
                updated += 1;
            }
        }
    } else if state.config.images.contains(&path.0) {
        for unit in units.iter_mut() {
            if unit.image == path.0 {
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

    fs::write(state.registered_file(), serde_json::to_vec(&*units)?)?;
    Ok(format!("{updated} computer(s) affected\n").customize())
}

#[get("/admin/image/{mac}/{image}")]
async fn image(
    path: Path<(String, String)>,
    state: Data<State>,
) -> Result<impl Responder, Box<dyn Error>> {
    let mut units = state.units.lock().unwrap();

    if !state.config.images.contains(&path.1) {
        return Ok(format!("Unknown image: {:?}", path.1)
            .to_owned()
            .customize()
            .with_status(StatusCode::BAD_REQUEST));
    }

    let mut updated = 0usize;

    if let Ok(mac) = path.0.parse::<MacAddr6>() {
        for unit in units.iter_mut() {
            if unit.mac == mac {
                unit.image = path.1.clone();
                updated += 1;
            }
        }
    } else if let Ok(ip) = path.0.parse::<Ipv4Addr>() {
        for unit in units.iter_mut() {
            if unit.static_ip() == ip {
                unit.image = path.1.clone();
                updated += 1;
            }
        }
    } else if path.0 == "all" {
        for unit in units.iter_mut() {
            unit.image = path.1.clone();
            updated += 1;
        }
    } else if let Some(&group) = state.config.groups.get_by_first(&path.0) {
        for unit in units.iter_mut() {
            if unit.group == group {
                unit.image = path.1.clone();
                updated += 1;
            }
        }
    } else if state.config.images.contains(&path.0) {
        for unit in units.iter_mut() {
            if unit.image == path.0 {
                unit.image = path.1.clone();
                updated += 1;
            }
        }
    } else {
        return Ok("Unknown PC"
            .to_owned()
            .customize()
            .with_status(StatusCode::BAD_REQUEST));
    }

    fs::write(state.registered_file(), serde_json::to_vec(&*units)?)?;
    Ok(format!("{updated} computer(s) affected\n").customize())
}

#[get("/admin/config")]
async fn get_config(state: Data<State>) -> Result<impl Responder, actix_web::Error> {
    Ok(serde_json::to_string(&state.config))
}

#[get("/admin/units")]
async fn get_units(state: Data<State>) -> Result<impl Responder, actix_web::Error> {
    Ok(serde_json::to_string(&*state.units.lock().unwrap()))
}

pub async fn main(state: Arc<State>) -> Result<()> {
    let HttpConfig {
        max_payload,
        listen_on,
        ref password,
    } = state.config.http;

    let pw = password.clone();

    let admin = state.storage_dir.join("admin");
    let data: Data<_> = state.into();

    HttpServer::new(move || {
        let pw = pw.clone();
        App::new()
            .wrap(Logger::default())
            .wrap(HttpAuthentication::basic(move |req, credentials| {
                let pw = pw.clone();
                async move {
                    if credentials.password() != Some(&pw) {
                        Err((ErrorUnauthorized("password incorrect"), req))
                    } else {
                        Ok(req)
                    }
                }
            }))
            .app_data(PayloadConfig::new(max_payload))
            .app_data(data.clone())
            .service(action)
            .service(image)
            .service(get_config)
            .service(get_units)
            .service(Files::new("/", &admin).index_file("index.html"))
    })
    .bind(listen_on)?
    .run()
    .await?;

    Ok(())
}
