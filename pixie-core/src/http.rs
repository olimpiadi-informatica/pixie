use std::{
    collections::BTreeMap,
    error::Error,
    fs,
    io::{self, BufRead, BufReader},
    net::{IpAddr, Ipv4Addr},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Mutex, RwLock},
    time::{SystemTime, UNIX_EPOCH},
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
use anyhow::{anyhow, bail, Context, Result};
use macaddr::MacAddr6;
use serde::{Deserialize, Serialize};

use pixie_shared::{Group, RegistrationInfo, Station, StationKind};

use crate::dnsmasq::DnsmasqHandle;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub max_payload: usize,
    pub listen_address: String,
    pub listen_port: u16,
}

#[derive(Debug, Deserialize, Clone)]
pub struct BootConfig {
    default: String,
    modes: BTreeMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Unit {
    mac: MacAddr6,
    kind: StationKind,
    row: u8,
    col: u8,
    group: u8,
    action: String,
}

#[derive(Debug)]
struct Machines {
    inner: Vec<Unit>,
    map: BTreeMap<MacAddr6, usize>,
}

impl Machines {
    fn new(inner: Vec<Unit>) -> Self {
        let map = inner.iter().enumerate().map(|(i, x)| (x.mac, i)).collect();
        Self { inner, map }
    }
}

#[derive(Clone, Debug)]
struct BootString(String);

#[derive(Clone, Debug)]
struct StorageDir(PathBuf);

#[derive(Clone, Debug)]
struct RegisteredFile(PathBuf);

#[derive(Clone, Debug)]
struct Groups(Vec<String>);

fn find_mac(ip: IpAddr) -> Result<MacAddr6> {
    struct Zombie {
        inner: Child,
    }

    impl Drop for Zombie {
        fn drop(&mut self) {
            self.inner.kill().unwrap();
            self.inner.wait().unwrap();
        }
    }

    if ip == IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)) {
        bail!("localhost not supported");
    }

    let s = ip.to_string();

    let mut child = Zombie {
        inner: Command::new("ip")
            .arg("neigh")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?,
    };
    let stdout = child.inner.stdout.take().unwrap();
    let lines = BufReader::new(stdout).lines();

    for line in lines {
        let line = line?;
        let mut parts = line.split(' ');

        if parts.next() == Some(&s) {
            let mac = parts.nth(3).unwrap();
            return Ok(mac.parse().unwrap());
        }
    }

    bail!("Mac address not found");
}

#[get("/boot.ipxe")]
async fn boot(
    req: HttpRequest,
    boot_config: Data<BootConfig>,
    machines: Data<RwLock<Machines>>,
) -> Result<impl Responder, Box<dyn Error>> {
    let peer_mac = match req.peer_addr() {
        Some(ip) => find_mac(ip.ip())?,
        _ => {
            return Ok("Specify an IPv4 address"
                .to_owned()
                .customize()
                .with_status(StatusCode::BAD_REQUEST))
        }
    };

    let Machines { inner, map } = &*machines
        .read()
        .map_err(|_| anyhow!("machines mutex is poisoned"))?;
    let unit = map.get(&peer_mac).copied();
    let mode: &str = unit
        .map(|unit| &inner[unit].action)
        .unwrap_or(&boot_config.default);
    let cmd = boot_config
        .modes
        .get(mode)
        .ok_or_else(|| anyhow!("mode {} does not exists", mode))?
        .replace("<server_loc>", req.app_config().host());
    let cmd = match unit {
        Some(unit) => cmd.replace(
            "<image>",
            match inner[unit].kind {
                StationKind::Worker => "worker",
                StationKind::Contestant => "contestant",
            },
        ),
        None => cmd,
    };
    Ok(cmd.customize())
}

#[get("/action/{mac}/{value}")]
async fn action(
    path: Path<(String, String)>,
    boot_config: Data<BootConfig>,
    machines: Data<RwLock<Machines>>,
    groups: Data<Groups>,
    registered_file: Data<RegisteredFile>,
) -> Result<impl Responder, Box<dyn Error>> {
    let Machines { inner, map } = &mut *machines
        .write()
        .map_err(|_| anyhow!("machines mutex is poisoned"))?;

    let value = &path.1;
    if boot_config.modes.get(value).is_none() {
        return Ok(format!("Unknown action {}", value)
            .customize()
            .with_status(StatusCode::BAD_REQUEST));
    }

    if let Ok(mac) = path.0.parse() {
        if let Some(&unit) = map.get(&mac) {
            inner[unit].action = value.clone();
            fs::write(&registered_file.0, serde_json::to_string(&inner)?)?;
            Ok("".to_owned().customize())
        } else {
            Ok("Unknown MAC address"
                .to_owned()
                .customize()
                .with_status(StatusCode::BAD_REQUEST))
        }
    } else if let Ok(group) = groups.0.binary_search(&path.0) {
        for unit in inner.iter_mut() {
            if unit.group as usize == group {
                unit.action = value.clone();
            }
        }
        fs::write(&registered_file.0, serde_json::to_string(&inner)?)?;
        Ok("".to_owned().customize())
    } else {
        Ok("Unknown PC"
            .to_owned()
            .customize()
            .with_status(StatusCode::BAD_REQUEST))
    }
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
    req: HttpRequest,
    body: Bytes,
    boot_config: Data<BootConfig>,
    hint: Data<Mutex<Station>>,
    machines: Data<RwLock<Machines>>,
    registered_file: Data<RegisteredFile>,
    groups: Data<Groups>,
    dnsmasq_handle: Data<Mutex<DnsmasqHandle>>,
) -> Result<impl Responder, Box<dyn Error>> {
    let body = body.to_vec();
    if let Ok(s) = std::str::from_utf8(&body) {
        if let Ok(data) = serde_json::from_str::<Station>(s) {
            if data.group as usize >= groups.0.len() {
                return Ok("Invalid group"
                    .customize()
                    .with_status(StatusCode::BAD_REQUEST));
            }

            let mut guard = hint.lock().map_err(|_| anyhow!("hint mutex is poisoned"))?;
            *guard = Station {
                kind: data.kind,
                row: data.row,
                col: data.col + 1,
                group: data.group,
            };

            let peer_ip = req.peer_addr().context("could not get peer ip")?.ip();
            let peer_mac = find_mac(peer_ip)?;

            let Machines { inner, map } = &mut *machines
                .write()
                .map_err(|_| anyhow!("machines mutex is poisoned"))?;

            let &mut unit = map
                .entry(peer_mac)
                .and_modify(|&mut unit| {
                    inner[unit].kind = data.kind;
                    inner[unit].row = data.row;
                    inner[unit].col = data.col;
                    inner[unit].group = data.group;
                })
                .or_insert_with(|| {
                    inner.push(Unit {
                        mac: peer_mac,
                        kind: data.kind,
                        row: data.row,
                        col: data.col,
                        group: data.group,
                        action: boot_config.default.clone(),
                    });
                    inner.len() - 1
                });

            let ip = Ipv4Addr::new(10, data.group, data.row, data.col);

            let mut dnsmasq_lock = dnsmasq_handle
                .lock()
                .map_err(|_| anyhow!("dnsmasq_handle mutex is poisoned"))?;
            dnsmasq_lock
                .write_host(unit, peer_mac, ip)
                .context("writing hosts file")?;
            dnsmasq_lock.send_sighup().context("sending sighup")?;

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
    mut groups: Vec<String>,
    mut dnsmasq_handle: DnsmasqHandle,
) -> Result<()> {
    let static_files = storage_dir.join("httpstatic");
    let images = storage_dir.join("images");
    let chunks = storage_dir.join("chunks");
    let registered_file = RegisteredFile(storage_dir.join("registered.json"));
    let storage_dir = StorageDir(storage_dir);
    let hint = Data::new(Mutex::new(Station::default()));
    let machines = Machines::new(units);
    for (i, unit) in machines.inner.iter().enumerate() {
        dnsmasq_handle.write_host(
            i,
            unit.mac,
            Ipv4Addr::new(10, unit.group, unit.row, unit.col),
        )?;
        if boot_config.modes.get(&unit.action).is_none() {
            bail!("Unknown mode {}", unit.action);
        }
    }
    dnsmasq_handle.send_sighup()?;
    let dnsmasq_handle = Data::new(Mutex::new(dnsmasq_handle));
    let machines = Data::new(RwLock::new(machines));
    groups.sort();
    let groups = Data::new(Groups(groups));

    HttpServer::new(move || {
        App::new()
            .wrap(Logger::default())
            .app_data(PayloadConfig::new(config.max_payload))
            .app_data(Data::new(boot_config.clone()))
            .app_data(Data::new(registered_file.clone()))
            .app_data(Data::new(storage_dir.clone()))
            .app_data(hint.clone())
            .app_data(machines.clone())
            .app_data(groups.clone())
            .app_data(dnsmasq_handle.clone())
            .service(upload_chunk)
            .service(upload_image)
            .service(Files::new("/static", &static_files))
            .service(Files::new("/image", &images))
            .service(Files::new("/chunk", &chunks))
            .service(boot)
            .service(get_registration_info)
            .service(register)
            .service(register_hint)
            .service(action)
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
    groups: Vec<String>,
    dnsmasq_handle: DnsmasqHandle,
) -> Result<()> {
    main(
        storage_dir,
        config,
        boot_config,
        units,
        groups,
        dnsmasq_handle,
    )
    .await
}
