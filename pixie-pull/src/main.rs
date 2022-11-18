use std::{
    collections::HashMap,
    fmt::Write as fmtWrite,
    fs::{self, File},
    io::{self, ErrorKind, Read, Seek, SeekFrom, Write},
    path::Path,
    thread,
    time::Duration,
};

use anyhow::{ensure, Result};
use clap::Parser;
use rand::RngCore;
use zstd::bulk;

use pixie_shared::{ChunkHash, Segment, CHUNK_SIZE};

#[derive(Parser, Debug)]
struct Options {
    #[clap(short, long, value_parser)]
    source: String,
}

trait FileFetcher {
    fn fetch_chunk(&self, hash: ChunkHash) -> Result<Vec<u8>>;
    fn fetch_image(&self) -> Result<Vec<pixie_shared::File>>;
}

struct LocalFileFetcher {
    path: String,
}

impl LocalFileFetcher {
    fn new(path: String) -> Self {
        Self { path }
    }
}

impl FileFetcher for LocalFileFetcher {
    fn fetch_chunk(&self, hash: ChunkHash) -> Result<Vec<u8>> {
        let mut hex = String::new();
        for byte in hash {
            write!(hex, "{:02x}", byte)?;
        }
        let path = Path::new(&self.path).join("chunks").join(hex);
        let data = std::fs::read(path)?;
        Ok(data)
    }

    fn fetch_image(&self) -> Result<Vec<pixie_shared::File>> {
        let info_path = Path::new(&self.path).join("info");
        let json = std::fs::read(info_path)?;
        let files = serde_json::from_str(std::str::from_utf8(&json)?)?;
        Ok(files)
    }
}

struct RemoteFileFetcher {
    url: String,
}

impl RemoteFileFetcher {
    fn new(url: String) -> Self {
        RemoteFileFetcher { url }
    }
}

impl FileFetcher for RemoteFileFetcher {
    fn fetch_chunk(&self, hash: ChunkHash) -> Result<Vec<u8>> {
        let mut hex = String::new();
        for byte in hash {
            write!(hex, "{:02x}", byte)?;
        }

        let resp = loop {
            let url = reqwest::Url::parse(&self.url)?
                .join("/chunk/")?
                .join(&hex)?;
            let resp = reqwest::blocking::get(url)?;
            if resp.status() != 418 {
                break resp;
            }
            thread::sleep(Duration::from_millis(
                1000 + rand::thread_rng().next_u64() % 1000,
            ));
        };

        ensure!(
            resp.status().is_success(),
            "failed to fetch chunk: status ({}) is not success",
            resp.status().as_u16(),
        );
        let body = resp.bytes()?;
        Ok(bulk::decompress(body.as_ref(), CHUNK_SIZE)?)
    }

    fn fetch_image(&self) -> Result<Vec<pixie_shared::File>> {
        let resp = reqwest::blocking::get(&self.url)?;
        ensure!(
            resp.status().is_success(),
            "failed to fetch image: status ({}) is not success",
            resp.status().as_u16()
        );
        let body = resp.text()?;
        let files = serde_json::from_str(&body)?;
        Ok(files)
    }
}

fn main() -> Result<()> {
    let args = Options::parse();

    ensure!(!args.source.is_empty(), "Specify a source");

    let file_fetcher: Box<dyn FileFetcher> =
        if args.source.starts_with("http://") || args.source.starts_with("https://") {
            Box::new(RemoteFileFetcher::new(args.source))
        } else {
            Box::new(LocalFileFetcher::new(args.source))
        };

    let mut stdout = io::stdout().lock();

    let info = file_fetcher.fetch_image()?;

    for pixie_shared::File { name, chunks } in info {
        if let Some(prefix) = name.parent() {
            fs::create_dir_all(prefix)?;
        }

        let mut file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .open(&name)?;

        let mut seen = HashMap::new();

        let total = chunks.len();

        let printable_name: &str = &name.to_string_lossy();

        for (idx, Segment { hash, start, size }) in chunks.into_iter().enumerate() {
            write!(
                stdout,
                " pulling chunk {idx} out of {total} to file '{printable_name}'\r"
            )?;
            stdout.flush()?;

            let mut data = vec![0; size];

            match seen.entry(hash) {
                std::collections::hash_map::Entry::Occupied(entry) => {
                    let s = *entry.get();
                    file.seek(SeekFrom::Start(s as u64))?;
                    file.read_exact(&mut data)?;
                    file.seek(SeekFrom::Start(start as u64))?;
                    file.write_all(&data)?;
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    file.seek(SeekFrom::Start(start as u64))?;
                    match file.read_exact(&mut data) {
                        Ok(()) => {
                            let cur_hash = blake3::hash(&data);
                            if &hash == cur_hash.as_bytes() {
                                continue;
                            }
                        }
                        Err(e) if e.kind() == ErrorKind::UnexpectedEof => {}
                        Err(e) => return Err(e.into()),
                    }

                    let data = file_fetcher.fetch_chunk(hash)?;
                    file.seek(SeekFrom::Start(start as u64))?;
                    file.write_all(&data)?;

                    entry.insert(start);
                }
            }
        }
        writeln!(stdout)?;
    }

    Ok(())
}
