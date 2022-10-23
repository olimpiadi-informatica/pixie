use std::{
    collections::BTreeMap,
    error::Error,
    fs,
    io::{self, BufRead, BufReader},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Mutex, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};

use actix_files::Files;
use actix_web::{
    get,
    http::StatusCode,
    middleware::Logger,
    post,
    web::{Bytes, Data, Json, Path},
    App, HttpRequest, HttpServer, Responder,
};
use anyhow::{anyhow, bail, Context, Result};
use ipnet::Ipv4Net;
use serde::{Deserialize, Serialize};

use pixie_shared::{Group, RegistrationInfo, Station, StationKind};

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

type MacAddr = [u8; 6];

#[derive(Debug, Serialize, Deserialize)]
pub struct Unit {
    mac: MacAddr,
    kind: StationKind,
    row: u32,
    col: u32,
}

#[derive(Debug)]
struct Machines {
    inner: Vec<Unit>,
    map: BTreeMap<MacAddr, usize>,
}

impl Machines {
    fn new(inner: Vec<Unit>) -> Self {
        let map = inner.iter().enumerate().map(|(i, x)| (x.mac, i)).collect();
        Self { inner, map }
    }
}

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

fn find_mac(ip: IpAddr) -> Result<MacAddr> {
    if ip == IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)) {
        bail!("localhost not supported");
    }

    let s = ip.to_string();

    let child = Command::new("ip")
        .arg("neigh")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child.stdout.unwrap();
    let lines = BufReader::new(stdout).lines();

    for line in lines {
        let line = line?;
        let mut parts = line.split(' ');

        if parts.next() == Some(&s) {
            let mut octets = parts.nth(3).unwrap().split(':');
            return Ok([(); 6].map(|()| u8::from_str_radix(octets.next().unwrap(), 16).unwrap()));
        }
    }

    bail!("Mac address not found");
}

#[post("/register")]
async fn register(
    req: HttpRequest,
    body: Bytes,
    hint: Data<Mutex<Station>>,
    machines: Data<RwLock<Machines>>,
    registered_file: Data<RegisteredFile>,
) -> Result<impl Responder, Box<dyn Error>> {
    let body = body.to_vec();
    if let Ok(s) = std::str::from_utf8(&body) {
        if let Ok(data) = serde_json::from_str::<Station>(s) {
            let mut guard = hint.lock().map_err(|_| anyhow!("hint mutex is poisoned"))?;
            *guard = Station {
                kind: data.kind,
                row: data.row,
                col: data.col + 1,
            };

            let peer_ip = req.peer_addr().context("could not get peer ip")?.ip();
            let peer_mac = find_mac(peer_ip)?;

            let Machines { inner, map } = &mut *machines
                .write()
                .map_err(|_| anyhow!("machines mutex is poisoned"))?;

            map.entry(peer_mac)
                .and_modify(|&mut unit| {
                    inner[unit].row = data.row;
                    inner[unit].col = data.col;
                    inner[unit].kind = data.kind;
                })
                .or_insert_with(|| {
                    inner.push(Unit {
                        mac: peer_mac,
                        kind: data.kind,
                        row: data.row,
                        col: data.col,
                    });
                    inner.len() - 1
                });

            fs::write(&registered_file.0, serde_json::to_string(&inner)?)?;
            return Ok("".customize());
        }
    }

    Ok("Invalid payload"
        .customize()
        .with_status(StatusCode::BAD_REQUEST))
}

#[get("/register_hint")]
async fn register_hint(hint: Data<Mutex<Station>>) -> Result<impl Responder, Box<dyn Error>> {
    let data = *hint.lock().map_err(|_| anyhow!("Mutex is poisoned"))?;
    Ok(Json(data))
}

#[post("/chunk")]
async fn upload_chunk(body: Bytes, storage_dir: Data<StorageDir>) -> io::Result<impl Responder> {
    let body = body.to_vec();
    let hash = blake3::hash(&body);
    let path = storage_dir.0.join("chunks").join(hash.to_hex().as_str());
    fs::write(path, body)?;
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
    fs::write(path, body)?;
    Ok("")
}

async fn main(
    storage_dir: PathBuf,
    config: Config,
    boot_config: BootConfig,
    units: Vec<Unit>,
) -> Result<()> {
    let static_files = storage_dir.join("httpstatic");
    let images = storage_dir.join("images");
    let chunks = storage_dir.join("chunks");
    let registered_file = RegisteredFile(storage_dir.join("registered.json"));
    let storage_dir = StorageDir(storage_dir);
    let hint = Data::new(Mutex::new(Station::default()));
    let machines = Data::new(RwLock::new(Machines::new(units)));

    HttpServer::new(move || {
        App::new()
            .wrap(Logger::default())
            .app_data(Data::new(boot_config.clone()))
            .app_data(Data::new(registered_file.clone()))
            .app_data(Data::new(storage_dir.clone()))
            .app_data(hint.clone())
            .app_data(machines.clone())
            .service(upload_chunk)
            .service(upload_image)
            .service(Files::new("/static", &static_files))
            .service(Files::new("/image", &images))
            .service(Files::new("/chunk", &chunks))
            .service(boot)
            .service(get_registration_info)
            .service(register)
            .service(register_hint)
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
    units: Vec<Unit>,
) -> Result<()> {
    main(storage_dir, config, boot_config, units).await
}
