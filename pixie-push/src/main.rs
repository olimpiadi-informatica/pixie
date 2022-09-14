use std::{
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Stdio},
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
    start: Offset,
    size: usize,
}

struct LocalFileSaver {
    path: String,
}

impl LocalFileSaver {
    fn get_chunk_path(path: &str) -> PathBuf {
        Path::new(path).join("chunks")
    }

    fn chunk_path(&self) -> PathBuf {
        LocalFileSaver::get_chunk_path(&self.path)
    }

    fn new(path: &str) -> Result<LocalFileSaver> {
        std::fs::create_dir_all(LocalFileSaver::get_chunk_path(path))?;
        Ok(LocalFileSaver {
            path: path.to_owned(),
        })
    }
}

impl FileSaver for LocalFileSaver {
    fn save_chunk(&self, data: &[u8]) -> Result<ChunkHash> {
        let hash = blake3::hash(data);
        std::fs::write(self.chunk_path().join(hash.to_hex().as_str()), data)?;
        Ok(hash.as_bytes().to_owned())
    }

    fn save_image(&self, info: Vec<pixie_core::shared::File>) -> Result<()> {
        let info_path = Path::new(&self.path).join("info");
        std::fs::write(info_path, serde_json::to_string(&info)?)?;
        Ok(())
    }
}

fn get_ext4_chunks(path: &str) -> Result<Option<Vec<ChunkInfo>>> {
    let child = Command::new("dumpe2fs")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child.stdout.unwrap();
    let mut lines = BufReader::new(stdout).lines();

    let block_size: usize = loop {
        let line = match lines.next() {
            Some(Ok(x)) => x,
            Some(Err(e)) => return Err(e.into()),
            None => return Ok(None),
        };

        if let Some(value) = line.strip_prefix("Block size:") {
            break value.trim().parse().unwrap();
        }
    };
    dbg!(block_size);

    let mut ans = Vec::new();

    let mut add = |b, e| {
        let mut b = b * block_size;
        let e = e * block_size;

        while b < e {
            ans.push(ChunkInfo {
                start: b,
                size: CHUNK_SIZE.min(e - b),
            });
            b += CHUNK_SIZE;
        }
    };

    loop {
        let (mut begin, end): (usize, usize) = loop {
            let line = match lines.next() {
                Some(Ok(x)) => x,
                Some(Err(e)) => return Err(e.into()),
                None => return Ok(Some(ans)),
            };

            if let Some(s) = line.strip_prefix("Group") {
                let a = s.find('(').unwrap();
                let b = s.find('-').unwrap();
                let c = s.find(')').unwrap();
                break (
                    s[a + 8..b].parse().unwrap(),
                    s[b + 1..c].parse::<usize>().unwrap() + 1,
                );
            }
        };

        loop {
            let line = lines.next().unwrap()?;

            if let Some(s) = line.strip_prefix("  Free blocks: ") {
                if !s.is_empty() {
                    for x in s.split(", ") {
                        let (a, b) = if let Some(m) = x.find('-') {
                            let a: usize = x[..m].parse().unwrap();
                            let b: usize = x[m + 1..].parse().unwrap();
                            (a, b + 1)
                        } else {
                            let a = x.parse().unwrap();
                            (a, a + 1)
                        };

                        if begin < a {
                            add(begin, a);
                        }
                        begin = b;
                    }
                }
                if begin < end {
                    add(begin, end);
                }
                break;
            }
        }
    }
}

fn get_file_chunks(path: &str) -> Result<Vec<ChunkInfo>> {
    let ext4_chunks = get_ext4_chunks(path)?;
    if let Some(chunks) = ext4_chunks {
        return Ok(chunks);
    }

    let mut file = File::open(path)?;
    let len = file.seek(SeekFrom::End(0))? as usize;

    Ok((0..((len + CHUNK_SIZE - 1) / CHUNK_SIZE))
        .map(|start| start * CHUNK_SIZE)
        .map(|start| ChunkInfo {
            start,
            size: (start + CHUNK_SIZE).min(len) - start,
        })
        .collect())
}

fn main() -> Result<()> {
    let args = Options::parse();

    ensure!(args.sources.len() > 0, "Specify at least one source");
    ensure!(args.destination.len() > 0, "Specify a destination");

    let file_saver: Box<dyn FileSaver> =
        if args.destination.starts_with("http://") || args.destination.starts_with("https://") {
            todo!("implement remote file saver")
        } else {
            Box::new(LocalFileSaver::new(&args.destination)?)
        };

    let mut info = Vec::<pixie_core::shared::File>::new();

    // TODO(veluca): parallelize.
    for s in args.sources {
        let chunks = get_file_chunks(&s)?;

        let mut file = std::fs::File::open(&s)?;

        let chunks: Result<Vec<_>> = chunks
            .into_iter()
            .map(|chnk| {
                file.seek(SeekFrom::Start(chnk.start as u64))?;
                let mut data = vec![0; chnk.size];
                file.read_exact(&mut data)?;
                let hash = file_saver.save_chunk(&data)?;
                Ok(pixie_core::shared::Segment {
                    hash,
                    start: chnk.start,
                    size: chnk.size,
                })
            })
            .collect();

        info.push(pixie_core::shared::File {
            name: Path::new(&s).to_owned(),
            chunks: chunks?,
        });
    }

    file_saver.save_image(info)
}
