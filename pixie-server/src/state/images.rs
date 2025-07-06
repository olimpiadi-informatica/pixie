use crate::state::{atomic_write, State, CHUNKS_DIR, IMAGES_DIR};
use anyhow::{ensure, Context, Result};
use pixie_shared::{ChunkHash, ChunkStats, ChunksStats, Image, ImagesStats, MAX_CHUNK_SIZE};
use tokio::sync::watch;

impl State {
    /// Checks whether the database contains the given chunk.
    pub fn has_chunk(&self, hash: ChunkHash) -> bool {
        self.chunks_stats
            .lock()
            .expect("chunks_stats lock is poisoned")
            .contains_key(&hash)
    }

    /// Get the chunk compressed data.
    pub fn get_chunk_cdata(&self, hash: ChunkHash) -> Result<Option<Vec<u8>>> {
        let path = self.storage_dir.join(CHUNKS_DIR).join(hex::encode(hash));
        let chunks_stats = self
            .chunks_stats
            .lock()
            .expect("chunks_stats lock is poisoned");
        let cdata = chunks_stats
            .contains_key(&hash)
            .then(|| std::fs::read(&path))
            .transpose()?;
        Ok(cdata)
    }

    /// Store the given chunk to the database.
    pub fn add_chunk(&self, data: &[u8]) -> Result<()> {
        let mut res = Ok(());
        let dec = lz4_flex::decompress(data, MAX_CHUNK_SIZE)?;
        ensure!(
            dec.len() <= MAX_CHUNK_SIZE,
            "Decompressed chunk size is too big: {}",
            dec.len()
        );
        let hash = *blake3::hash(&dec).as_bytes();
        let path = self.storage_dir.join(CHUNKS_DIR).join(hex::encode(hash));
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

    /// Finds and deletes all chunks which are not part of any image.
    pub fn gc_chunks(&self) -> Result<()> {
        let mut res = Ok(());
        self.images_stats.send_modify(|images_stats| {
            let mut chunks_stats = self
                .chunks_stats
                .lock()
                .expect("chunks_stats lock is poisoned");
            chunks_stats.retain(|k, v| {
                if res.is_ok() && v.ref_cnt == 0 {
                    let path = self.storage_dir.join(CHUNKS_DIR).join(hex::encode(k));
                    res = std::fs::remove_file(path);
                    if res.is_ok() {
                        images_stats.total_csize -= v.csize;
                        images_stats.reclaimable -= v.csize;
                        false
                    } else {
                        true
                    }
                } else {
                    true
                }
            });
        });
        Ok(res?)
    }

    pub fn get_image_serialized(&self, image: &str) -> Result<Option<Vec<u8>>> {
        ensure!(
            self.config.images.iter().any(|i| i == image),
            "Unknown image: {image}"
        );

        let path = self.storage_dir.join(IMAGES_DIR).join(image);
        match std::fs::read(path) {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Assumes that new_image is valid
    fn write_image(
        &self,
        name: String,
        new_image: &Image,
        images_stats: &mut ImagesStats,
        chunks_stats: &mut ChunksStats,
    ) -> Result<()> {
        let path = self.storage_dir.join(IMAGES_DIR).join(&name);

        let old_chunks = if images_stats.images.contains_key(&name) {
            let old_image = std::fs::read(&path)?;
            let old_image: Image =
                postcard::from_bytes(&old_image).expect("failed to deserialize image");
            old_image.disk
        } else {
            Vec::new()
        };

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
        ensure!(self.config.images.contains(&name), "Unknown image: {name}");

        let mut res = Ok(());
        self.images_stats.send_modify(|images_stats| {
            res = (|| {
                let mut chunks_stats = self
                    .chunks_stats
                    .lock()
                    .expect("chunks_stats lock is poisoned");
                for chunk in &image.disk {
                    ensure!(
                        chunks_stats.contains_key(&chunk.hash),
                        "chunk {} not found",
                        hex::encode(chunk.hash)
                    );
                }
                let now = chrono::Utc::now();
                let version = now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                let name_with_version = format!("{name}@{version}");
                self.write_image(name, image, images_stats, &mut chunks_stats)?;
                self.write_image(name_with_version, image, images_stats, &mut chunks_stats)?;
                Ok(())
            })();
        });
        res
    }

    pub fn rollback_image(&self, full_name: &str) -> Result<()> {
        let mut it = full_name.split('@');
        let name = it.next().expect("Invalid image name").to_owned();
        let _version = it.next().unwrap_or_default();
        ensure!(it.next().is_none(), "Invalid image name");
        ensure!(self.config.images.contains(&name), "Unknown image: {name}");

        let mut res = Ok(());
        let path = self.storage_dir.join(IMAGES_DIR).join(full_name);
        self.images_stats.send_modify(|images_stats| {
            res = (|| {
                let mut chunks_stats = self
                    .chunks_stats
                    .lock()
                    .expect("chunks_stats lock is poisoned");
                ensure!(
                    images_stats.images.contains_key(full_name),
                    "Unknown image: {full_name}"
                );
                let data = std::fs::read(&path)?;
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
        let name = it.next().expect("Invalid image name").to_owned();
        let _version = it.next().unwrap_or_default();
        ensure!(it.next().is_none(), "Invalid image name");
        ensure!(self.config.images.contains(&name), "Unknown image: {name}");

        let mut res = Ok(());
        self.images_stats.send_modify(|images_stats| {
            res = (|| {
                let mut chunks_stats = self
                    .chunks_stats
                    .lock()
                    .expect("chunks_stats lock is poisoned");
                ensure!(
                    images_stats.images.contains_key(full_name),
                    "Unknown image: {full_name}"
                );
                let path = self.storage_dir.join(IMAGES_DIR).join(full_name);
                let data = std::fs::read(&path)?;
                let image: Image =
                    postcard::from_bytes(&data).expect("failed to deserialize image");
                std::fs::remove_file(&path)?;
                images_stats.images.remove(full_name);
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
