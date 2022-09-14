use std::{
    fmt::Write as fmtWrite,
    fs::File,
    io::{ErrorKind, Read, Seek, SeekFrom, Write},
    path::Path,
};

use anyhow::{ensure, Result};
use clap::Parser;

use pixie_core::shared::ChunkHash;

#[derive(Parser, Debug)]
struct Options {
    #[clap(short, long, value_parser)]
    source: String,
}

trait FileFetcher {
    fn fetch_chunk(&self, hash: ChunkHash) -> Result<Vec<u8>>;
    fn fetch_image(&self) -> Result<Vec<pixie_core::shared::File>>;
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

    fn fetch_image(&self) -> Result<Vec<pixie_core::shared::File>> {
        let info_path = Path::new(&self.path).join("info");
        let json = std::fs::read(info_path)?;
        let files = serde_json::from_str(std::str::from_utf8(&json)?)?;
        Ok(files)
    }
}

fn main() -> Result<()> {
    let args = Options::parse();

    ensure!(!args.source.is_empty(), "Specify a source");

    let file_fetcher: Box<dyn FileFetcher> =
        if args.source.starts_with("http://") || args.source.starts_with("https://") {
            todo!("implement remote file fetcher")
        } else {
            Box::new(LocalFileFetcher::new(args.source))
        };

    let info = file_fetcher.fetch_image()?;

    for pixie_core::shared::File { name, chunks } in info {
        if let Some(prefix) = name.parent() {
            std::fs::create_dir_all(prefix)?;
        }

        let mut file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .open(&name)?;

        for pixie_core::shared::Segment { hash, start, size } in chunks {
            let mut data = vec![0; size];
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
        }
    }

    Ok(())
}
