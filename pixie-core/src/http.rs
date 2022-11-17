use std::{
    collections::BTreeMap,
    error::Error,
    fs,
    io::{self, BufRead, BufReader},
    net::{IpAddr, Ipv4Addr},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex, RwLock,
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
use anyhow::{anyhow, bail, Context, Result};
use interfaces::Interface;
use macaddr::MacAddr6;
use mktemp::Temp;
use serde::{Deserialize, Serialize};

use pixie_shared::{Station, StationKind};

use crate::dnsmasq::DnsmasqHandle;

#[derive(Clone, Debug, Deserialize)]
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
struct Groups(BTreeMap<String, u8>);

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

fn find_interface_ip(peer_ip: Ipv4Addr) -> Result<Ipv4Addr, Box<dyn Error>> {
    for interface in Interface::get_all()? {
        for address in &interface.addresses {
            let Some(IpAddr::V4(addr)) = address.addr.map(|x| x.ip()) else {
                continue;
            };
            let Some(IpAddr::V4(mask)) = address.mask.map(|x| x.ip()) else {
                continue;
            };
            if (u32::from_ne_bytes(addr.octets()) ^ u32::from_ne_bytes(peer_ip.octets()))
                & u32::from_ne_bytes(mask.octets())
                == 0
            {
                return Ok(addr);
            }
        }
    }
    Err(anyhow!("Could not find the corresponding ip"))?
}

#[get("/boot.ipxe")]
async fn boot(
    req: HttpRequest,
    boot_config: Data<BootConfig>,
    machines: Data<RwLock<Machines>>,
    config: Data<Config>,
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
        .ok_or_else(|| anyhow!("mode {} does not exists", mode))?;
    let IpAddr::V4(peer_ip) = req.peer_addr().unwrap().ip() else {
        Err(anyhow!("IPv6 is not supported"))?
    };
    let cmd = cmd
        .replace("<server_ip>", &find_interface_ip(peer_ip)?.to_string())
        .replace("<server_port>", &config.listen_port.to_string());
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

    let mut updated = 0usize;

    if let Ok(mac) = path.0.parse() {
        if let Some(&unit) = map.get(&mac) {
            inner[unit].action = value.clone();
            updated += 1;
        } else {
            return Ok("Unknown MAC address"
                .to_owned()
                .customize()
                .with_status(StatusCode::BAD_REQUEST));
        }
    } else if let Ok(ip) = path.0.parse::<Ipv4Addr>() {
        for unit in &mut *inner {
            if Ipv4Addr::new(10, unit.group, unit.row, unit.col) == ip {
                unit.action = value.clone();
                updated += 1;
            }
        }
    } else if path.0 == "all" {
        for unit in inner.iter_mut() {
            unit.action = value.clone();
            updated += 1;
        }
    } else if let Some(&group) = groups.0.get(&path.0) {
        for unit in inner.iter_mut() {
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

    fs::write(&registered_file.0, serde_json::to_string(&inner)?)?;
    Ok(format!("{updated} computer updated\n").customize())
}

#[post("/register")]
async fn register(
    req: HttpRequest,
    body: Bytes,
    boot_config: Data<BootConfig>,
    hint: Data<Mutex<Station>>,
    machines: Data<RwLock<Machines>>,
    registered_file: Data<RegisteredFile>,
    dnsmasq_handle: Data<Mutex<DnsmasqHandle>>,
) -> Result<impl Responder, Box<dyn Error>> {
    let body = body.to_vec();
    if let Ok(s) = std::str::from_utf8(&body) {
        if let Ok(data) = serde_json::from_str::<Station>(s) {
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

#[get("/has_chunk/{hash}")]
async fn has_chunk(hash: Path<String>, storage_dir: Data<StorageDir>) -> impl Responder {
    let path = storage_dir.0.join("chunks").join(&*hash);
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
    storage_dir: Data<StorageDir>,
) -> io::Result<impl Responder> {
    let path = storage_dir.0.join("chunks").join(&*hash);
    let tmp_file = Temp::new_file_in(storage_dir.0.join("tmp"))
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
    storage_dir: Data<StorageDir>,
) -> io::Result<impl Responder> {
    let body = body.to_vec();
    let path = storage_dir.0.join("images").join(&*name);
    fs::write(path, body)?;
    Ok("")
}

#[get("/chunk/{hash}")]
async fn get_chunk(
    hash: Path<String>,
    storage_dir: Data<StorageDir>,
) -> io::Result<impl Responder> {
    static CONN: AtomicUsize = AtomicUsize::new(0);

    struct Handle;

    impl Handle {
        fn new(limit: usize) -> Option<Self> {
            CONN.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |x| {
                (x < limit).then(|| x + 1)
            })
            .is_ok()
            .then(|| Handle)
        }
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            CONN.fetch_sub(1, Ordering::SeqCst);
        }
    }

    match Handle::new(12) {
        Some(_handle) => Ok(fs::read(storage_dir.0.join("chunks").join(&*hash))?.customize()),
        None => Ok(Vec::new().customize().with_status(StatusCode::IM_A_TEAPOT)),
    }
}

async fn main(
    storage_dir: PathBuf,
    config: Config,
    boot_config: BootConfig,
    units: Vec<Unit>,
    groups: BTreeMap<String, u8>,
    mut dnsmasq_handle: DnsmasqHandle,
) -> Result<()> {
    let static_files = storage_dir.join("httpstatic");
    let images = storage_dir.join("images");
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
    let groups = Data::new(Groups(groups));
    let config_data = Data::new(config.clone());

    HttpServer::new(move || {
        App::new()
            .wrap(Logger::default())
            .app_data(PayloadConfig::new(config.max_payload))
            .app_data(Data::new(boot_config.clone()))
            .app_data(Data::new(registered_file.clone()))
            .app_data(Data::new(storage_dir.clone()))
            .app_data(config_data.clone())
            .app_data(hint.clone())
            .app_data(machines.clone())
            .app_data(groups.clone())
            .app_data(dnsmasq_handle.clone())
            .service(has_chunk)
            .service(upload_chunk)
            .service(upload_image)
            .service(Files::new("/static", &static_files))
            .service(Files::new("/image", &images))
            .service(get_chunk)
            .service(boot)
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
    groups: BTreeMap<String, u8>,
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
