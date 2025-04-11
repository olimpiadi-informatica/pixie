use crate::state::{atomic_write, State};
use anyhow::{ensure, Context, Result};
use pixie_shared::{ChunkStats, Image, ImagesStats};
use std::collections::BTreeMap;
use tokio::sync::watch;

impl State {
    pub fn gc_chunks(&self) -> Result<()> {
        let mut res = Ok(());
        self.images_stats.send_modify(|images_stats| {
            let mut chunks_stats = self
                .chunks_stats
                .lock()
                .expect("chunks_stats lock is poisoned");
            chunks_stats.retain(|k, v| {
                if res.is_ok() && v.ref_cnt == 0 {
                    let path = self.storage_dir.join("chunks").join(hex::encode(k));
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
            let old_image = std::fs::read(&path)?;
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
        let name = it.next().expect("Invalid image name").to_owned();
        let _version = it.next().unwrap_or_default();
        ensure!(it.next().is_none(), "Invalid image name");
        ensure!(self.config.images.contains(&name), "Unknown image: {name}",);

        let mut res = Ok(());
        let path = self.storage_dir.join("images").join(full_name);
        self.images_stats.send_modify(|images_stats| {
            res = (|| {
                let mut chunks_stats = self
                    .chunks_stats
                    .lock()
                    .expect("chunks_stats lock is poisoned");
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
        let _name = it.next().expect("Invalid image name").to_owned();
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
                let data = std::fs::read(&path)?;
                let image: Image =
                    postcard::from_bytes(&data).expect("failed to deserialize image");
                std::fs::remove_file(&path)?;
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
