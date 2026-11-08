//! The database for pixie-server.
//!
//! Structure of the storage directory:
//! - `config.yaml`: configuration file for pixie-server
//! - `registered.json`: json file containing all information about registered units.
//! - `admin/`: directory containing the static files for the admin web interface.
//! - `chunks/`: directory containing the image's chunks.
//! - `images/`: directory containing the image's info.
//! - `tftpboot/`: directory containing the necessary files for network boot.

#![warn(clippy::unwrap_used)]

mod images;
mod units;

use anyhow::{Context, Result, anyhow, ensure};
use macaddr::MacAddr6;
use pixie_shared::{
    ChunkHash, ChunkStats, ChunksStats, Config, Image, ImagesStats, RegistrationInfo, Unit,
};
use std::{
    collections::HashMap,
    fs::File,
    io::{ErrorKind, Write},
    net::{IpAddr, Ipv4Addr},
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub use units::UnitSelector;

const CONFIG_YAML: &str = "config.yaml";
const REGISTERED_JSON: &str = "registered.json";
const CHUNKS_DIR: &str = "chunks";
const IMAGES_DIR: &str = "images";

/// Atomically write `data` at the specified `path`.
///
/// On crash `path` is guaranteed to be in a consistent state, but a temporary file might be left
/// behind.
fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    static CNT: AtomicU64 = AtomicU64::new(0);

    let (tmp_path, mut file) = loop {
        let tmp_path =
            path.with_file_name(format!("{:x}.tmp", CNT.fetch_add(1, Ordering::Relaxed)));
        match File::options().write(true).create_new(true).open(&tmp_path) {
            Ok(file) => break (tmp_path, file),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => Err(e)?,
        }
    };
    file.write_all(data)
        .with_context(|| format!("write to file: {}", tmp_path.display()))?;
    std::mem::drop(file);
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Builds a map from ip address to hostname parsing the hostfile at `path`.
fn build_hostmap(path: Option<&Path>) -> Result<HashMap<Ipv4Addr, String>> {
    let mut hostmap = HashMap::new();
    if let Some(path) = path {
        let hosts =
            hostfile::parse_file(path).map_err(|e| anyhow!("Error parsing host file: {e}"))?;
        for mut host in hosts {
            if let IpAddr::V4(ip) = host.ip {
                let old = hostmap.insert(ip, std::mem::take(&mut host.names[0]));
                if old.is_some() {
                    log::warn!("Duplicated hostname for {ip}");
                }
            } else {
                log::warn!(
                    "ignoring non-IPv4 address {} for host {}",
                    host.ip,
                    host.names[0]
                );
            }
        }
    }
    Ok(hostmap)
}

/// See [the module-level documentation][self].
pub struct State {
    /// The storage_dir received by command line arguments.
    pub storage_dir: PathBuf,
    /// The config parsed from the config file.
    config: watch::Sender<Config>,
    /// The hostmap built from the hostmap file.
    hostmap: watch::Sender<HashMap<Ipv4Addr, String>>,

    units: watch::Sender<Vec<Unit>>,
    registration_hint: Mutex<Option<RegistrationInfo>>,
    images_stats: watch::Sender<ImagesStats>,
    chunks_stats: Mutex<ChunksStats>,
    dhcp_leases: Mutex<HashMap<Ipv4Addr, MacAddr6>>,

    /// Token to shutdown the entire server.
    pub cancel_token: CancellationToken,
}

impl State {
    fn read_config_and_hostmap(
        storage_dir: &Path,
        units: &[Unit],
    ) -> Result<(Config, HashMap<Ipv4Addr, String>)> {
        let config: Config = {
            let path = storage_dir.join(CONFIG_YAML);
            let file = File::open(&path)
                .with_context(|| format!("open config file: {}", path.display()))?;
            serde_yaml::from_reader(file)
                .with_context(|| format!("deserialize config from {}", path.display()))?
        };

        let hostmap = build_hostmap(config.hosts.hostsfile.as_deref())?;

        for unit in units {
            ensure!(
                config.groups.get_by_second(&unit.group).is_some(),
                "unit group {} not found in config",
                unit.group,
            );
            ensure!(
                config.images.contains(&unit.image),
                "unit image {} not found in config",
                unit.image,
            );
        }

        Ok((config, hostmap))
    }

    /// Loads the [`State`] from the given path.
    pub fn load(storage_dir: PathBuf) -> Result<Self> {
        let units_path = storage_dir.join(REGISTERED_JSON);
        let units: Vec<Unit> = if units_path.exists() {
            let file = File::open(&units_path)
                .with_context(|| format!("open units file: {}", units_path.display()))?;
            serde_json::from_reader(&file)
                .with_context(|| format!("deserialize units from {}", units_path.display()))?
        } else {
            Vec::new()
        };

        let (config, hostmap) = Self::read_config_and_hostmap(&storage_dir, &units)?;
        let config = watch::Sender::new(config);
        let hostmap = watch::Sender::new(hostmap);
        let units = watch::Sender::new(units);

        let mut units_rx = units.subscribe();
        tokio::spawn(async move {
            while units_rx.changed().await.is_ok() {
                let units = units_rx.borrow_and_update().clone();
                let json = serde_json::to_vec(&units).expect("serialize units");
                // TODO(virv): handle error
                atomic_write(&units_path, &json).expect("write units file");
            }
        });

        let chunks_dir = storage_dir.join(CHUNKS_DIR);
        let mut chunks_stats: ChunksStats = std::fs::read_dir(&chunks_dir)
            .with_context(|| format!("open chunks dir: {}", chunks_dir.display()))?
            .map(|file| {
                let file = file?;
                let metadata = file.metadata()?;
                let csize = metadata.len();

                let name = file
                    .file_name()
                    .to_str()
                    .and_then(|s| hex::decode(s).ok())
                    .and_then(|s| ChunkHash::try_from(&s[..]).ok())
                    .with_context(|| format!("invalid chunk name: {:?}", file.file_name()))?;

                Ok((name, ChunkStats { csize, ref_cnt: 0 }))
            })
            .collect::<Result<_>>()?;

        let images_dir = storage_dir.join(IMAGES_DIR);
        let images = std::fs::read_dir(&images_dir)
            .with_context(|| format!("open images dir: {}", images_dir.display()))?
            .map(|image_entry| {
                let image_entry = image_entry?;
                let path = image_entry.path();
                let image_name = image_entry
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow!("invalid image name {:?}", image_entry.file_name()))?;
                let content = std::fs::read(&path)
                    .with_context(|| format!("read image file: {}", path.display()))?;
                let image = postcard::from_bytes::<Image>(&content)
                    .with_context(|| format!("deserialize image from {}", path.display()))?;
                for chunk in &image.disk {
                    let chunk_stats = chunks_stats
                        .get_mut(&chunk.hash)
                        .with_context(|| format!("chunk {} not found", hex::encode(chunk.hash)))?;
                    chunk_stats.ref_cnt += 1;
                }
                Ok((image_name, (image.size(), image.csize())))
            })
            .collect::<Result<_>>()?;

        let reclaimable = chunks_stats
            .values()
            .filter(|stat| stat.ref_cnt == 0)
            .map(|stat| stat.csize)
            .sum();
        let total_csize = chunks_stats.values().map(|stat| stat.csize).sum();

        let images_stats = ImagesStats {
            total_csize,
            reclaimable,
            images,
        };

        let cancel_token = CancellationToken::new();

        Ok(Self {
            storage_dir,
            config,
            hostmap,
            units,
            registration_hint: Mutex::new(None),
            images_stats: watch::Sender::new(images_stats),
            chunks_stats: Mutex::new(chunks_stats),
            dhcp_leases: Mutex::new(HashMap::new()),
            cancel_token,
        })
    }

    pub fn config(&self) -> watch::Ref<'_, Config> {
        self.config.borrow()
    }

    pub fn hostmap(&self) -> watch::Ref<'_, HashMap<Ipv4Addr, String>> {
        self.hostmap.borrow()
    }

    pub fn subscribe_config(&self) -> watch::Receiver<Config> {
        self.config.subscribe()
    }

    pub fn subscribe_hostmap(&self) -> watch::Receiver<HashMap<Ipv4Addr, String>> {
        self.hostmap.subscribe()
    }

    /// Reloads `config.yaml` and `hostsfile` from disk.
    pub fn reload(&self) -> Result<()> {
        let units = self.units.borrow().clone();
        let (config, hostmap) = Self::read_config_and_hostmap(&self.storage_dir, &units)?;
        self.config.send_if_modified(|old| {
            if *old != config {
                *old = config;
                true
            } else {
                false
            }
        });
        self.hostmap.send_if_modified(|old| {
            if *old != hostmap {
                *old = hostmap;
                true
            } else {
                false
            }
        });
        Ok(())
    }

    pub fn record_dhcp_lease(&self, ip: Ipv4Addr, mac: MacAddr6) {
        let mut leases = self.dhcp_leases.lock().expect("dhcp_leases mutex poisoned");
        leases.retain(|_, existing_mac| *existing_mac != mac);
        leases.insert(ip, mac);
    }

    pub fn release_dhcp_lease(&self, mac: MacAddr6) {
        self.dhcp_leases
            .lock()
            .expect("dhcp_leases mutex poisoned")
            .retain(|_, existing_mac| *existing_mac != mac);
    }

    pub fn allocate_dhcp_ip(
        &self,
        mac: MacAddr6,
        low: Ipv4Addr,
        high: Ipv4Addr,
    ) -> Option<Ipv4Addr> {
        let mut leases = self.dhcp_leases.lock().expect("dhcp_leases mutex poisoned");
        let range = u32::from(low)..=u32::from(high);
        if let Some((&ip, _)) = leases
            .iter()
            .find(|&(&ip, &m)| m == mac && range.contains(&u32::from(ip)))
        {
            return Some(ip);
        }

        let units = self.units.borrow();
        for raw in range {
            let ip = Ipv4Addr::from(raw);
            if !leases.contains_key(&ip) && !units.iter().any(|u| u.static_ip() == ip) {
                leases.retain(|_, existing_mac| *existing_mac != mac);
                leases.insert(ip, mac);
                return Some(ip);
            }
        }
        None
    }

    pub fn lookup_mac(&self, ip: Ipv4Addr) -> Option<MacAddr6> {
        if let Some(&mac) = self
            .dhcp_leases
            .lock()
            .expect("dhcp_leases mutex poisoned")
            .get(&ip)
        {
            return Some(mac);
        }
        self.units
            .borrow()
            .iter()
            .find(|u| u.static_ip() == ip)
            .map(|u| u.mac)
    }
}
