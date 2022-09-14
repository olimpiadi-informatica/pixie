use std::{collections::HashMap, path::PathBuf, time::SystemTime};

use actix_files::Files;
use actix_web::{
    get, middleware::Logger, web::Data, web::Json, App, HttpRequest, HttpServer, Responder,
};
use anyhow::Result;
use serde::Deserialize;

use pixie_shared::{Group, RegistrationInfo};

#[derive(Debug, Deserialize)]
pub struct Config {
    pub listen_address: String,
    pub listen_port: u16,
}

#[derive(Debug, Deserialize)]
pub struct BootConfig {
    pub current: String,
    pub modes: HashMap<String, String>,
}

#[derive(Clone, Debug)]
struct BootString(String);

#[get("/boot.ipxe")]
async fn boot(_: HttpRequest, boot_string: Data<BootString>) -> impl Responder {
    boot_string.0.clone()
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

async fn main(storage_dir: PathBuf, config: Config, boot_string: String) -> Result<()> {
    let static_files = storage_dir.join("httpstatic");
    let images = storage_dir.join("images");
    let chunks = storage_dir.join("chunks");
    let boot_string = BootString(boot_string);
    HttpServer::new(move || {
        App::new()
            .wrap(Logger::default())
            .app_data(Data::new(boot_string.clone()))
            .service(Files::new("/static", &static_files))
            .service(Files::new("/image", &images))
            .service(Files::new("/chunk", &chunks))
            .service(boot)
            .service(get_registration_info)
    })
    .bind((config.listen_address, config.listen_port))?
    .run()
    .await?;
    Ok(())
}

#[actix_web::main]
pub async fn main_sync(storage_dir: PathBuf, config: Config, boot_string: String) -> Result<()> {
    main(storage_dir, config, boot_string).await
}
