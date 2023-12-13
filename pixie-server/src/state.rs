use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File},
    net::{IpAddr, Ipv4Addr},
    path::PathBuf,
    sync::Mutex,
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use tokio::{
    sync::{watch, Mutex as AsyncMutex},
    time,
};

use pixie_shared::{ChunkStat, Config, Image, ImageStat, Station, Unit};

const CONFIG: &str = "config.yaml";
const UNITS: &str = "registered.json";

pub struct State {
    pub storage_dir: PathBuf,
    pub config: Config,
    pub hostmap: HashMap<Ipv4Addr, String>,

    pub units: watch::Sender<Vec<Unit>>,
    // TODO: use an Option
    pub last: Mutex<Station>,
    pub image_stats: AsyncMutex<ImageStat>,
    pub chunk_stats: AsyncMutex<BTreeMap<[u8; 32], ChunkStat>>,
}

impl State {
    pub fn registered_file(&self) -> PathBuf {
        self.storage_dir.join(UNITS)
    }

    pub fn load(storage_dir: PathBuf) -> Result<Self> {
        let config: Config = {
            let path = storage_dir.join(CONFIG);
            let file = File::open(&path)
                .with_context(|| format!("open config file: {}", path.display()))?;
            serde_yaml::from_reader(file)
                .with_context(|| format!("deserialize config from {}", path.display()))?
        };

        let mut hostmap = HashMap::new();
        if let Some(hostsfile) = &config.dhcp.hostsfile {
            let hosts = hostfile::parse_file(hostsfile)
                .map_err(|e| anyhow!("Error parsing host file: {e}"))?;
            for host in hosts {
                if let IpAddr::V4(ip) = host.ip {
                    hostmap.insert(ip, host.names[0].clone());
                }
            }
        }

        let units_path = storage_dir.join(UNITS);
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
                let mut file = File::create(&units_path).unwrap();
                serde_json::to_writer(&mut file, &units).unwrap();
                time::sleep(Duration::from_secs(1)).await;
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

        let images: BTreeMap<String, (u64, u64)> = config
            .images
            .iter()
            .map(|image_name| {
                let path = storage_dir.join("images").join(image_name);
                if !path.is_file() {
                    return Ok((image_name.clone(), (0, 0)));
                }
                let content = fs::read(&path)?;
                let image = postcard::from_bytes::<Image>(&content)?;
                let mut size = 0;
                let mut csize = 0;
                for chunk in image.disk {
                    size += chunk.size as u64;
                    csize += chunk.csize as u64;
                    chunk_stats.get_mut(&chunk.hash).unwrap().ref_cnt += 1;
                }
                Ok((image_name.clone(), (size, csize)))
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
            chunk_stats: AsyncMutex::new(chunk_stats),
            image_stats: AsyncMutex::new(ImageStat {
                total_csize,
                reclaimable,
                images,
            }),
        })
    }
}
