use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File},
    net::{IpAddr, Ipv4Addr},
    path::{Path, PathBuf},
    sync::Mutex,
};

use anyhow::{anyhow, bail, Context, Result};
use macaddr::MacAddr6;
use mktemp::Temp;
use tokio::sync::watch;

use pixie_shared::{ChunkHash, ChunkStat, Config, Image, ImageStat, Station, Unit};

fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    // TODO(virv): find a better way to make a temporary file
    let tmp_file = Temp::new_file_in(path.parent().unwrap())?.release();
    fs::write(&tmp_file, data)?;
    fs::rename(&tmp_file, path)?;
    Ok(())
}

fn get_image_csize(image: &Image) -> u64 {
    let mut chunks: Vec<_> = image
        .disk
        .iter()
        .map(|chunk| (chunk.hash, chunk.csize))
        .collect();
    chunks.sort_unstable_by_key(|(hash, _)| *hash);
    chunks.dedup_by_key(|(hash, _)| *hash);
    chunks.into_iter().map(|(_, size)| size as u64).sum()
}

pub struct State {
    pub storage_dir: PathBuf,
    pub config: Config,
    pub hostmap: watch::Sender<HashMap<Ipv4Addr, String>>,

    pub units: watch::Sender<Vec<Unit>>,
    // TODO: use an Option
    pub last: Mutex<Station>,
    pub image_stats: watch::Sender<ImageStat>,
    pub chunk_stats: Mutex<BTreeMap<[u8; 32], ChunkStat>>,
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

        let mut chunk_stats: BTreeMap<[u8; 32], ChunkStat> =
            fs::read_dir(storage_dir.join("chunks"))
                .unwrap()
                .map(|file| {
                    let file = file?;
                    let metadata = file.metadata().unwrap();
                    let csize = metadata.len();

                    let name = file.file_name();
                    let name = hex::decode(name.to_str().unwrap()).unwrap();
                    let name = <[u8; 32]>::try_from(&name[..]).unwrap();

                    Ok((name, ChunkStat { csize, ref_cnt: 0 }))
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
                let csize = get_image_csize(&image);
                let mut size = 0;
                for chunk in image.disk {
                    size += chunk.size as u64;
                    chunk_stats.get_mut(&chunk.hash).unwrap().ref_cnt += 1;
                }
                Ok((image_name, (size, csize)))
            })
            .collect::<Result<_>>()?;

        let reclaimable = chunk_stats
            .values()
            .filter(|stat| stat.ref_cnt == 0)
            .map(|stat| stat.csize)
            .sum();
        let total_csize = chunk_stats.values().map(|stat| stat.csize).sum();

        Ok(Self {
            storage_dir,
            config,
            hostmap,
            units,
            last,
            image_stats: watch::Sender::new(ImageStat {
                total_csize,
                reclaimable,
                images,
            }),
            chunk_stats: Mutex::new(chunk_stats),
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

    pub fn gc_chunks(&self) -> Result<()> {
        self.image_stats.send_modify(|image_stats| {
            let mut chunk_stats = self.chunk_stats.lock().unwrap();
            let mut cnt = 0;
            chunk_stats.retain(|k, v| {
                if v.ref_cnt == 0 {
                    let path = self.storage_dir.join("chunks").join(hex::encode(k));
                    fs::remove_file(path).unwrap();
                    image_stats.total_csize -= v.csize;
                    image_stats.reclaimable -= v.csize;
                    cnt += 1;
                    false
                } else {
                    true
                }
            });
        });
        Ok(())
    }

    pub fn add_chunk(&self, hash: ChunkHash, data: &[u8]) -> Result<()> {
        let path = self.storage_dir.join("chunks").join(hex::encode(hash));

        self.image_stats.send_modify(|image_stats| {
            let mut chunk_stats = self.chunk_stats.lock().unwrap();

            let chunk = ChunkStat {
                csize: data.len() as u64,
                ref_cnt: 0,
            };
            let ins = chunk_stats.insert(hash, chunk).is_none();
            if ins {
                atomic_write(&path, data).unwrap();
                image_stats.total_csize += data.len() as u64;
                image_stats.reclaimable += data.len() as u64;
            }
        });

        Ok(())
    }

    pub fn add_image(&self, name: String, image: Image) -> Result<()> {
        if !self.config.images.contains(&name) {
            bail!("Unknown image: {}", name);
        }

        let now = chrono::Utc::now();
        let name_pinned = format!(
            "{}-{}",
            name,
            now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        );
        let data = postcard::to_allocvec(&image)?;

        let size = image.disk.iter().map(|chunk| chunk.size as u64).sum();
        let csize = get_image_csize(&image);

        self.image_stats.send_modify(|image_stats| {
            let mut chunk_stats = self.chunk_stats.lock().unwrap();

            for name in [name, name_pinned] {
                let path = self.storage_dir.join("images").join(&name);
                if path.exists() {
                    let old_image = fs::read(&path).unwrap();
                    let old_image = postcard::from_bytes::<Image>(&old_image).unwrap();
                    for chunk in old_image.disk {
                        let info = chunk_stats.get_mut(&chunk.hash).unwrap();
                        info.ref_cnt -= 1;
                        if info.ref_cnt == 0 {
                            image_stats.reclaimable += info.csize;
                        }
                    }
                }

                atomic_write(&path, &data).unwrap();
                image_stats.images.insert(name, (size, csize));
                for chunk in &image.disk {
                    let info = chunk_stats.get_mut(&chunk.hash).unwrap();
                    if info.ref_cnt == 0 {
                        image_stats.reclaimable -= info.csize;
                    }
                    info.ref_cnt += 1;
                }
            }
        });

        Ok(())
    }

    pub fn set_ping(&self, peer_mac: MacAddr6, time: u64, message: Vec<u8>) {
        self.units.send_if_modified(|units| {
            let Some(unit) = units.iter_mut().find(|unit| unit.mac == peer_mac) else {
                log::warn!("Got ping from unknown unit");
                return false;
            };

            unit.last_ping_timestamp = time;
            unit.last_ping_msg = message;

            true
        });
    }
}
