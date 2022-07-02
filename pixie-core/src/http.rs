use std::{path::PathBuf, time::SystemTime};

use actix_files::Files;
use actix_web::{get, middleware::Logger, web::Json, App, HttpRequest, HttpServer, Responder};
use anyhow::Result;
use serde::Deserialize;

use crate::shared::{Group, RegistrationInfo};

#[derive(Debug, Deserialize)]
pub struct Config {
    listen_address: String,
    listen_port: u16,
}

#[get("/boot.ipxe")]
async fn boot(_: HttpRequest) -> impl Responder {
    /*
    if let Some(val) = req.peer_addr() {
        println!("Address {:?}", val.ip());
    };
    */

    "#!ipxe
chain /static/reboot.efi"
}

#[get("/get_registration_info")]
async fn get_registration_info(_: HttpRequest) -> impl Responder {
    let t = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_micros();
    let ans = RegistrationInfo {
        groups: vec![
            Group {
                name: "workers".into(),
                shape: None,
            },
            Group {
                name: "contestants".into(),
                shape: Some((10, 10)),
            },
        ],
        candidate_group: "contestants".into(),
        candidate_position: vec![2, (t as u32 % 10) as u8],
    };
    Json(ans)
}

async fn main(storage_dir: PathBuf, config: Config) -> Result<()> {
    let static_files = storage_dir.join("httpstatic").to_owned();
    HttpServer::new(move || {
        App::new()
            .wrap(Logger::default())
            .service(Files::new("/static", &static_files))
            .service(boot)
            .service(get_registration_info)
    })
    .bind((config.listen_address, config.listen_port))?
    .run()
    .await?;
    Ok(())
}

#[actix_web::main]
pub async fn main_sync(storage_dir: PathBuf, config: Config) -> Result<()> {
    main(storage_dir, config).await
}
