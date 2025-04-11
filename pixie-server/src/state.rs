use anyhow::{anyhow, ensure, Context, Result};
use macaddr::MacAddr6;
use mktemp::Temp;
use pixie_shared::{ChunkStats, ChunksStats, Config, Image, ImagesStats, Station, Unit};
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
                let name = <[u8; 32]>::try_from(&name[..]).unwrap();

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
                let mut size = 0;
                for chunk in image.disk {
                    size += chunk.size as u64;
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

    pub fn set_unit_ping(&self, peer_mac: MacAddr6, time: u64, message: Vec<u8>) {
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

    pub fn get_last(&self) -> Station {
        self.last.lock().expect("last mutex is poisoned").clone()
    }

    pub fn set_last(&self, station: Station) {
        *self.last.lock().expect("last mutex is poisoned") = station;
    }

    pub fn gc_chunks(&self) {
        self.images_stats.send_modify(|images_stats| {
            let mut chunks_stats = self
                .chunks_stats
                .lock()
                .expect("chunks_stats lock is poisoned");
            chunks_stats.retain(|k, v| {
                if v.ref_cnt == 0 {
                    let path = self.storage_dir.join("chunks").join(hex::encode(k));
                    // TODO(virv): handle errors
                    fs::remove_file(path).unwrap();
                    images_stats.total_csize -= v.csize;
                    images_stats.reclaimable -= v.csize;
                    false
                } else {
                    true
                }
            });
        });
    }

    pub fn add_chunk(&self, data: &[u8]) -> Result<()> {
        let mut res = Ok(());
        let hash = *blake3::hash(data).as_bytes();
        let path = self.storage_dir.join("chunks").join(hex::encode(hash));
        self.images_stats.send_if_modified(|images_stats| {
            res = (|| {
                let mut chunks_stats = self
                    .chunks_stats
                    .lock()
                    .expect("chunks_stats lock is poisoned");
                let chunk = ChunkStats {
                    csize: data.len() as u64,
                    ref_cnt: 0,
                };
                let ins = chunks_stats.insert(hash, chunk).is_none();
                if ins {
                    atomic_write(&path, data)?;
                    images_stats.total_csize += data.len() as u64;
                    images_stats.reclaimable += data.len() as u64;
                }
                Ok(())
            })();
            res.is_ok()
        });
        res
    }

    fn write_image(
        &self,
        name: String,
        new_image: &Image,
        images_stats: &mut ImagesStats,
        chunks_stats: &mut BTreeMap<[u8; 32], ChunkStats>,
    ) -> Result<()> {
        let path = self.storage_dir.join("images").join(&name);

        let old_chunks = if images_stats.images.contains_key(&name) {
            let old_image = fs::read(&path)?;
            let old_image: Image =
                postcard::from_bytes(&old_image).expect("failed to deserialize image");
            old_image.disk
        } else {
            Vec::new()
        };

        for chunk in &new_image.disk {
            ensure!(
                chunks_stats.contains_key(&chunk.hash),
                "chunk {} not found",
                hex::encode(chunk.hash)
            );
        }

        let data = postcard::to_allocvec(&new_image).expect("failed to serialize image");
        atomic_write(&path, &data).context("failed to write image")?;

        images_stats
            .images
            .insert(name, (new_image.size(), new_image.csize()));

        for chunk in &new_image.disk {
            let info = chunks_stats.get_mut(&chunk.hash).expect("chunk not found");
            if info.ref_cnt == 0 {
                images_stats.reclaimable -= info.csize;
            }
            info.ref_cnt += 1;
        }

        for chunk in &old_chunks {
            let info = chunks_stats.get_mut(&chunk.hash).expect("chunk not found");
            info.ref_cnt -= 1;
            if info.ref_cnt == 0 {
                images_stats.reclaimable += info.csize;
            }
        }

        Ok(())
    }

    pub fn add_image(&self, name: String, image: &Image) -> Result<()> {
        ensure!(self.config.images.contains(&name), "Unknown image: {name}",);

        let mut res = Ok(());
        self.images_stats.send_modify(|images_stats| {
            res = (|| {
                let mut chunks_stats = self
                    .chunks_stats
                    .lock()
                    .expect("chunks_stats lock is poisoned");
                let now = chrono::Utc::now();
                let version = now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                let name_with_version = format!("{}@{}", name, version);
                self.write_image(name, image, images_stats, &mut chunks_stats)?;
                self.write_image(name_with_version, image, images_stats, &mut chunks_stats)?;
                Ok(())
            })();
        });
        res
    }

    pub fn rollback_image(&self, full_name: &str) -> Result<()> {
        let mut it = full_name.split('@');
        let name = it.next().unwrap().to_owned();
        let _version = it.next().unwrap_or_default();
        ensure!(it.next().is_none(), "Invalid image name");
        ensure!(self.config.images.contains(&name), "Unknown image: {name}",);

        let mut res = Ok(());
        let path = self.storage_dir.join("images").join(full_name);
        self.images_stats.send_modify(|images_stats| {
            res = (|| {
                let mut chunks_stats = self.chunks_stats.lock().unwrap();
                let data = fs::read(&path)?;
                let image =
                    postcard::from_bytes::<Image>(&data).expect("failed to deserialize image");
                self.write_image(name, &image, images_stats, &mut chunks_stats)?;
                Ok(())
            })();
        });
        res
    }

    pub fn delete_image(&self, full_name: &str) -> Result<()> {
        let mut it = full_name.split('@');
        let _name = it.next().unwrap().to_owned();
        let _version = it.next().unwrap_or_default();
        ensure!(it.next().is_none(), "Invalid image name");

        let mut res = Ok(());
        self.images_stats.send_modify(|images_stats| {
            res = (|| {
                let mut chunks_stats = self
                    .chunks_stats
                    .lock()
                    .expect("chunks_stats lock is poisoned");
                let old = images_stats.images.remove(full_name);
                if old.is_none() {
                    return Ok(());
                }
                let path = self.storage_dir.join("images").join(full_name);
                let data = fs::read(&path)?;
                let image: Image =
                    postcard::from_bytes(&data).expect("failed to deserialize image");
                fs::remove_file(&path)?;
                for chunk in image.disk {
                    let info = chunks_stats.get_mut(&chunk.hash).expect("chunk not found");
                    info.ref_cnt -= 1;
                    if info.ref_cnt == 0 {
                        images_stats.reclaimable += info.csize;
                    }
                }
                Ok(())
            })();
        });
        res
    }

    pub fn subscribe_images(&self) -> watch::Receiver<ImagesStats> {
        self.images_stats.subscribe()
    }
}
