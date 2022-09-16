use std::{
    fs::File,
    io::{self, Write},
    net::SocketAddr,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use actix_files::Files;
use actix_web::{
    get, http::StatusCode, middleware::Logger, post, web::Bytes, web::Data, web::Json, web::Path,
    App, HttpRequest, HttpServer, Responder,
};
use anyhow::Result;
use ipnet::Ipv4Net;
use serde::Deserialize;

use pixie_shared::{Group, RegistrationInfo, Station};

#[derive(Debug, Deserialize)]
pub struct Config {
    pub listen_address: String,
    pub listen_port: u16,
}

#[derive(Debug, Deserialize, Clone)]
pub struct BootOption {
    net: Ipv4Net,
    cmd: String,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(transparent)]
pub struct BootConfig {
    options: Vec<BootOption>,
}

#[derive(Clone, Debug)]
struct BootString(String);

#[derive(Clone, Debug)]
struct StorageDir(PathBuf);

#[derive(Clone, Debug)]
struct RegisteredFile(PathBuf);

#[get("/boot.ipxe")]
async fn boot(req: HttpRequest, boot_config: Data<BootConfig>) -> impl Responder {
    let peer_ip = match req.peer_addr() {
        Some(SocketAddr::V4(ip)) => *ip.ip(),
        _ => {
            return "Specify an IPv4 address"
                .to_owned()
                .customize()
                .with_status(StatusCode::BAD_REQUEST);
        }
    };

    for BootOption { net, cmd } in &boot_config.options {
        if net.contains(&peer_ip) {
            return cmd.clone().customize();
        }
    }

    "No cmd specified for this IP"
        .to_owned()
        .customize()
        .with_status(StatusCode::INTERNAL_SERVER_ERROR)
}

#[get("/get_registration_info")]
async fn get_registration_info() -> impl Responder {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
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

#[post("/register")]
async fn register(
    body: Bytes,
    registered_file: Data<RegisteredFile>,
) -> io::Result<impl Responder> {
    let body = body.to_vec();
    if let Ok(s) = std::str::from_utf8(&body) {
        if let Ok(data) = serde_json::from_str::<Station>(s) {
            let mut file = File::options()
                .append(true)
                .create(true)
                .open(&registered_file.0)?;

            writeln!(file, "{}", serde_json::to_string(&data)?)?;
            return Ok("".customize());
        }
    }

    Ok("Invalid payload"
        .customize()
        .with_status(StatusCode::BAD_REQUEST))
}

#[post("/chunk")]
async fn upload_chunk(body: Bytes, storage_dir: Data<StorageDir>) -> io::Result<impl Responder> {
    let body = body.to_vec();
    let hash = blake3::hash(&body);
    let path = storage_dir.0.join("chunks").join(hash.to_hex().as_str());
    std::fs::write(path, body)?;
    Ok("")
}

#[post("/image/{name}")]
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

async fn main(storage_dir: PathBuf, config: Config, boot_config: BootConfig) -> Result<()> {
    let static_files = storage_dir.join("httpstatic");
    let images = storage_dir.join("images");
    let chunks = storage_dir.join("chunks");
    let unix_time = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let registered_file = RegisteredFile(storage_dir.join(format!("registered_{}", unix_time)));
    let storage_dir = StorageDir(storage_dir);
    HttpServer::new(move || {
        App::new()
            .wrap(Logger::default())
            .app_data(Data::new(boot_config.clone()))
            .app_data(Data::new(registered_file.clone()))
            .app_data(Data::new(storage_dir.clone()))
            .service(upload_chunk)
            .service(upload_image)
            .service(Files::new("/static", &static_files))
            .service(Files::new("/image", &images))
            .service(Files::new("/chunk", &chunks))
            .service(boot)
            .service(get_registration_info)
            .service(register)
    })
    .bind((config.listen_address, config.listen_port))?
    .run()
    .await?;
    Ok(())
}

#[actix_web::main]
pub async fn main_sync(
    storage_dir: PathBuf,
    config: Config,
    boot_config: BootConfig,
) -> Result<()> {
    main(storage_dir, config, boot_config).await
}
