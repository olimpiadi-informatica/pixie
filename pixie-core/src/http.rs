use std::{collections::HashMap, io, path::PathBuf, time::SystemTime};

use actix_files::Files;
use actix_web::{
    get, middleware::Logger, post, web::Bytes, web::Data, web::Json, web::Path, App, HttpRequest,
    HttpServer, Responder,
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

#[derive(Clone, Debug)]
struct StorageDir(PathBuf);

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

#[post("/upload_chunk")]
async fn upload_chunk(body: Bytes, storage_dir: Data<StorageDir>) -> io::Result<impl Responder> {
    let body = body.to_vec();
    let hash = blake3::hash(&body);
    let path = storage_dir.0.join("chunks").join(hash.to_hex().as_str());
    std::fs::write(path, body)?;
    Ok("")
}

#[post("/upload_image/{name}")]
async fn upload_image(
    name: Path<String>,
    body: Bytes,
    storage_dir: Data<StorageDir>,
) -> io::Result<impl Responder> {
    let body = body.to_vec();
    let path = storage_dir.0.join("images").join(&*name);
    std::fs::write(path, body)?;
    Ok("")
}

async fn main(storage_dir: PathBuf, config: Config, boot_string: String) -> Result<()> {
    let static_files = storage_dir.join("httpstatic");
    let images = storage_dir.join("images");
    let chunks = storage_dir.join("chunks");
    let boot_string = BootString(boot_string);
    let storage_dir = StorageDir(storage_dir);
    HttpServer::new(move || {
        App::new()
            .wrap(Logger::default())
            .app_data(Data::new(boot_string.clone()))
            .app_data(Data::new(storage_dir.clone()))
            .service(Files::new("/static", &static_files))
            .service(Files::new("/image", &images))
            .service(Files::new("/chunk", &chunks))
            .service(boot)
            .service(get_registration_info)
            .service(upload_chunk)
            .service(upload_image)
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
