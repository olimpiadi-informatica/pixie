mod images;
mod units;

use anyhow::{anyhow, Context, Result};
use mktemp::Temp;
use pixie_shared::{ChunkHash, ChunkStats, ChunksStats, Config, Image, ImagesStats, Station, Unit};
use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File},
    net::{IpAddr, Ipv4Addr},
    path::{Path, PathBuf},
    sync::Mutex,
};
use tokio::sync::watch;

fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    // TODO(virv): find a better way to make a temporary file
    let tmp_file = Temp::new_file_in(path.parent().unwrap())?.release();
    fs::write(&tmp_file, data)?;
    fs::rename(&tmp_file, path)?;
    Ok(())
}

pub struct State {
    pub storage_dir: PathBuf,
    pub config: Config,
    hostmap: watch::Sender<HashMap<Ipv4Addr, String>>,

    pub units: watch::Sender<Vec<Unit>>,
    // TODO: use an Option
    last: Mutex<Station>,
    images_stats: watch::Sender<ImagesStats>,
    chunks_stats: Mutex<ChunksStats>,
}

impl State {
    pub fn load(storage_dir: PathBuf) -> Result<Self> {
        let config: Config = {
            let path = storage_dir.join("config.yaml");
            let file = File::open(&path)
                .with_context(|| format!("open config file: {}", path.display()))?;
            serde_yaml::from_reader(file)
                .with_context(|| format!("deserialize config from {}", path.display()))?
        };

        let mut hostmap = HashMap::new();
        if let Some(hostsfile) = &config.hosts.hostsfile {
            let hosts = hostfile::parse_file(hostsfile)
                .map_err(|e| anyhow!("Error parsing host file: {e}"))?;
            for host in hosts {
                if let IpAddr::V4(ip) = host.ip {
                    hostmap.insert(ip, host.names[0].clone());
                }
            }
        }
        let hostmap = watch::Sender::new(hostmap);

        let units_path = storage_dir.join("registered.json");
        let units = watch::Sender::new({
            if units_path.exists() {
                let file = File::open(&units_path).with_context(|| {
                    format!("open registered.json file: {}", units_path.display())
                })?;
                serde_json::from_reader(&file).with_context(|| {
                    format!("deserialize registered.json from {}", units_path.display())
                })?
            } else {
                Vec::new()
            }
        });

        let mut units_rx = units.subscribe();
        tokio::spawn(async move {
            while units_rx.changed().await.is_ok() {
                let units = units_rx.borrow_and_update().clone();
                let json = serde_json::to_vec(&units).unwrap();
                atomic_write(&units_path, &json).unwrap();
            }
        });

        let last = Mutex::new(Station {
            group: config.groups.iter().next().unwrap().0.clone(),
            image: config.images[0].clone(),
            ..Default::default()
        });

        let mut chunks_stats: ChunksStats = fs::read_dir(storage_dir.join("chunks"))
            .unwrap()
            .map(|file| {
                let file = file?;
                let metadata = file.metadata().unwrap();
                let csize = metadata.len();

                let name = file.file_name();
                let name = hex::decode(name.to_str().unwrap()).unwrap();
                let name = ChunkHash::try_from(&name[..]).unwrap();

                Ok((name, ChunkStats { csize, ref_cnt: 0 }))
            })
            .collect::<Result<_>>()?;

        let images: BTreeMap<String, (u64, u64)> = fs::read_dir(storage_dir.join("images"))
            .unwrap()
            .map(|image_entry| {
                let image_entry = image_entry?;
                let image_name = image_entry.file_name().into_string().unwrap();
                let path = image_entry.path();
                let content = fs::read(&path)?;
                let image = postcard::from_bytes::<Image>(&content)?;
                let csize = image.csize();
                let size = image.size();
                for chunk in image.disk {
                    chunks_stats.get_mut(&chunk.hash).unwrap().ref_cnt += 1;
                }
                Ok((image_name, (size, csize)))
            })
            .collect::<Result<_>>()?;

        let reclaimable = chunks_stats
            .values()
            .filter(|stat| stat.ref_cnt == 0)
            .map(|stat| stat.csize)
            .sum();
        let total_csize = chunks_stats.values().map(|stat| stat.csize).sum();

        Ok(Self {
            storage_dir,
            config,
            hostmap,
            units,
            last,
            images_stats: watch::Sender::new(ImagesStats {
                total_csize,
                reclaimable,
                images,
            }),
            chunks_stats: Mutex::new(chunks_stats),
        })
    }

    pub fn reload(&self) -> Result<()> {
        let mut hostmap = HashMap::new();
        if let Some(hostsfile) = &self.config.hosts.hostsfile {
            let hosts = hostfile::parse_file(hostsfile)
                .map_err(|e| anyhow!("Error parsing host file: {e}"))?;
            for host in hosts {
                if let IpAddr::V4(ip) = host.ip {
                    hostmap.insert(ip, host.names[0].clone());
                }
            }
        }
        self.hostmap.send_replace(hostmap);
        Ok(())
    }

    pub fn subscribe_hostmap(&self) -> watch::Receiver<HashMap<Ipv4Addr, String>> {
        self.hostmap.subscribe()
    }
}
