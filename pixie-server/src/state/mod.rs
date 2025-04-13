#![warn(clippy::unwrap_used)]

mod images;
mod units;

use anyhow::{anyhow, ensure, Context, Result};
use pixie_shared::{ChunkHash, ChunkStats, ChunksStats, Config, Image, ImagesStats, Station, Unit};
use std::{
    collections::HashMap,
    fs::File,
    io::{ErrorKind, Write},
    net::{IpAddr, Ipv4Addr},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
};
use tokio::sync::watch;

pub use units::UnitSelector;

const CONFIG_YAML: &str = "config.yaml";
const REGISTERED_JSON: &str = "registered.json";
const CHUNKS_DIR: &str = "chunks";
const IMAGES_DIR: &str = "images";

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

fn build_hostmap(path: Option<&Path>) -> Result<HashMap<Ipv4Addr, String>> {
    let mut hostmap = HashMap::new();
    if let Some(path) = path {
        let hosts =
            hostfile::parse_file(path).map_err(|e| anyhow!("Error parsing host file: {e}"))?;
        for mut host in hosts {
            if let IpAddr::V4(ip) = host.ip {
                hostmap.insert(ip, std::mem::take(&mut host.names[0]));
            }
        }
    }
    Ok(hostmap)
}

pub struct State {
    pub storage_dir: PathBuf,
    pub config: Config,
    hostmap: watch::Sender<HashMap<Ipv4Addr, String>>,

    units: watch::Sender<Vec<Unit>>,
    // TODO: use an Option
    last: Mutex<Station>,
    images_stats: watch::Sender<ImagesStats>,
    chunks_stats: Mutex<ChunksStats>,
}

impl State {
    pub fn load(storage_dir: PathBuf) -> Result<Self> {
        let config: Config = {
            let path = storage_dir.join(CONFIG_YAML);
            let file = File::open(&path)
                .with_context(|| format!("open config file: {}", path.display()))?;
            serde_yaml::from_reader(file)
                .with_context(|| format!("deserialize config from {}", path.display()))?
        };

        let hostmap = build_hostmap(config.hosts.hostsfile.as_deref())?;

        let units_path = storage_dir.join(REGISTERED_JSON);
        let units: Vec<Unit> = if units_path.exists() {
            let file = File::open(&units_path)
                .with_context(|| format!("open units file: {}", units_path.display()))?;
            serde_json::from_reader(&file)
                .with_context(|| format!("deserialize units from {}", units_path.display()))?
        } else {
            Vec::new()
        };
        for unit in &units {
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

        let last = Station {
            group: config
                .groups
                .iter()
                .next()
                .context("there should be at least one group")?
                .0
                .clone(),
            image: config
                .images
                .first()
                .context("there should be at least one image")?
                .clone(),
            ..Default::default()
        };

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

        Ok(Self {
            storage_dir,
            config,
            hostmap: watch::Sender::new(hostmap),
            units,
            last: Mutex::new(last),
            images_stats: watch::Sender::new(images_stats),
            chunks_stats: Mutex::new(chunks_stats),
        })
    }

    pub fn reload(&self) -> Result<()> {
        let hostmap = build_hostmap(self.config.hosts.hostsfile.as_deref())?;
        self.hostmap.send_replace(hostmap);
        Ok(())
    }

    pub fn subscribe_hostmap(&self) -> watch::Receiver<HashMap<Ipv4Addr, String>> {
        self.hostmap.subscribe()
    }
}
