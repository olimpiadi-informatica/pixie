use std::{
    fs::File,
    io::{Seek, SeekFrom},
};

use anyhow::{ensure, Result};
use clap::Parser;

use pixie_core::shared::{ChunkHash, Offset};

const CHUNK_SIZE: usize = 1 << 22;

#[derive(Parser, Debug)]
struct Options {
    #[clap(short, long, value_parser)]
    destination: String,
    #[clap(last = true, value_parser)]
    sources: Vec<String>,
}

trait FileSaver {
    fn save_chunk(&self, data: &[u8]) -> Result<ChunkHash>;
    fn save_image(&self, info: Vec<pixie_core::shared::File>) -> Result<()>;
}

#[derive(Debug)]
struct ChunkInfo {
    path: String,
    start: Offset,
    size: usize,
}

fn get_ext4_chunks(path: &str) -> Result<Option<Vec<ChunkInfo>>> {
    // TODO
    Ok(None)
}

fn get_file_chunks(path: &str) -> Result<Vec<ChunkInfo>> {
    let ext4_chunks = get_ext4_chunks(path)?;
    if let Some(chunks) = ext4_chunks {
        return Ok(chunks);
    }

    let mut file = File::open(path)?;
    let len = file.seek(SeekFrom::End(0))? as usize;

    Ok((0..((len + CHUNK_SIZE - 1) / CHUNK_SIZE))
        .map(|start| ChunkInfo {
            path: path.into(),
            start,
            size: (start + CHUNK_SIZE).min(len) - start,
        })
        .collect())
}

fn main() -> Result<()> {
    let args = Options::parse();

    ensure!(args.sources.len() > 0, "Specify at least one source");
    ensure!(args.destination.len() > 0, "Specify a destination");

    for s in args.sources {
        let chunks = get_file_chunks(&s)?;
        println!("{} -> {:?}", s, &chunks[..10.min(chunks.len())]);
    }

    Ok(())
}
