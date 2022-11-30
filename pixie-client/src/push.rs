use std::{
    fs::File,
    io::{self, ErrorKind, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use anyhow::{ensure, Context, Result};
use clap::Parser;
use gpt::GptConfig;
use reqwest::{blocking::Client, Url};
use zstd::bulk;

use pixie_shared::{ChunkHash, Offset, Segment, CHUNK_SIZE};

#[derive(Parser, Debug)]
struct Options {
    #[clap(short, long, value_parser)]
    destination: String,
    #[clap(last = true, value_parser)]
    sources: Vec<String>,
}

trait FileSaver {
    fn save_chunk(&self, data: &[u8]) -> Result<ChunkHash>;
    fn save_image(&self, info: Vec<pixie_shared::File>) -> Result<()>;
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

    fn save_image(&self, info: Vec<pixie_shared::File>) -> Result<()> {
        let info_path = Path::new(&self.path).join("info");
        std::fs::write(info_path, serde_json::to_string(&info)?)?;
        Ok(())
    }
}

struct RemoteFileSaver {
    url: String,
}

impl RemoteFileSaver {
    fn new(url: String) -> Self {
        Self { url }
    }
}

impl FileSaver for RemoteFileSaver {
    fn save_chunk(&self, data: &[u8]) -> Result<ChunkHash> {
        let hash = blake3::hash(data);
        let hash_hex = hash.to_hex();

        let client = Client::new();
        let url = Url::parse(&self.url)?.join(&format!("/has_chunk/{}", hash_hex.as_str()))?;
        let resp = client
            .get(url)
            .send()
            .context("failed to check chunk existance")?;
        ensure!(
            resp.status().is_success(),
            "failed to check chunk existance"
        );

        if &*resp.bytes()? == b"pass" {
            return Ok(hash.as_bytes().to_owned());
        }

        let url = Url::parse(&self.url)?.join(&format!("/chunk/{}", hash_hex.as_str()))?;
        let resp = client
            .post(url)
            .body(data.to_owned())
            .send()
            .with_context(|| {
                format!(
                    "failed to upload chunk to server, chunk size {}",
                    data.len()
                )
            })?;
        ensure!(
            resp.status().is_success(),
            "failed to upload chunk server, status {}, chunk size {}",
            resp.status().as_u16(),
            data.len()
        );

        Ok(hash.as_bytes().to_owned())
    }

    fn save_image(&self, info: Vec<pixie_shared::File>) -> Result<()> {
        let client = Client::new();
        let data = serde_json::to_string(&info)?;
        let resp = client
            .post(&self.url)
            .body(data)
            .send()
            .context("failed to upload image to server")?;
        ensure!(
            resp.status().is_success(),
            "failed to upload image to server, status ({})",
            resp.status().as_u16(),
        );
        Ok(())
    }
}

fn get_ext4_chunks(path: &str) -> Result<Option<Vec<ChunkInfo>>> {
    fn le16(buf: &[u8], lo: usize) -> u16 {
        (0..2).map(|i| (buf[lo + i] as u16) << (8 * i)).sum()
    }

    fn le32(buf: &[u8], lo: usize) -> u32 {
        (0..4).map(|i| (buf[lo + i] as u32) << (8 * i)).sum()
    }

    fn le64_32_32(buf: &[u8], lo: usize, hi: usize) -> u64 {
        (0..4)
            .map(|i| ((buf[lo + i] as u64) << (8 * i)) + ((buf[hi + i] as u64) << (8 * i + 32)))
            .sum()
    }

    fn has_superblock(group: usize) -> bool {
        if group <= 1 {
            return true;
        }

        for d in [3, 5, 7] {
            let mut p = 1;
            while p < group {
                p *= d;
            }
            if p == group {
                return true;
            }
        }

        false
    }

    let mut reader = File::open(path).unwrap();
    let mut superblock = [0; 1024];
    reader.seek(SeekFrom::Start(1024))?;
    match reader.read_exact(&mut superblock) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => Err(e)?,
    }

    let magic = le16(&superblock, 0x38);
    if magic != 0xEF53 {
        return Ok(None);
    }

    let feature_incompat = le32(&superblock, 0x60);
    if feature_incompat & 0x80 == 0 {
        // INCOMPAT_64BIT flag
        return Ok(None);
    }

    let feature_ro_compat = le32(&superblock, 0x64);
    if feature_ro_compat & 0x1 == 0 {
        // RO_COMPAT_SPARSE_SUPER flag
        return Ok(None);
    }

    let blocks_count = le64_32_32(&superblock, 0x4, 0x150);
    let log_block_size = le32(&superblock, 0x18);
    assert!(blocks_count.checked_shl(10 + log_block_size).is_some());
    let block_size = 1u64 << (10 + log_block_size);

    let blocks_per_group = le32(&superblock, 0x20) as u64;
    let groups = (blocks_count + blocks_per_group - 1) / blocks_per_group;

    let first_data_block = le32(&superblock, 0x14) as u64;
    let desc_size = le16(&superblock, 0xfe) as u64;
    let reserved_gdt_blocks = le16(&superblock, 0xce);

    let blocks_for_special_group = 1
        + ((desc_size * groups + block_size - 1) / block_size) as usize
        + reserved_gdt_blocks as usize;

    let mut group_descriptors = vec![0; (desc_size * groups) as usize];
    let mut bitmap = vec![0; block_size as usize];
    reader
        .seek(SeekFrom::Start(block_size * (first_data_block + 1)))
        .unwrap();
    reader.read_exact(&mut group_descriptors)?;

    let mut ans = Vec::new();

    for (group, group_descriptor) in group_descriptors.chunks(desc_size as usize).enumerate() {
        let flags = le16(group_descriptor, 0x12);
        if flags & 0x2 != 0 {
            // EXT4_BG_BLOCK_UNINIT
            if has_superblock(group) {
                for block in 0..blocks_for_special_group {
                    if group * blocks_per_group as usize + block < blocks_count as usize {
                        ans.push(ChunkInfo {
                            start: block_size as usize
                                * (group * blocks_per_group as usize + block),
                            size: block_size as usize,
                        });
                    }
                }
            }
        } else {
            let block_bitmap = le64_32_32(group_descriptor, 0x0, 0x20);

            reader
                .seek(SeekFrom::Start(block_size * block_bitmap))
                .unwrap();
            reader.read_exact(&mut bitmap)?;

            for block in 0..8 * block_size as usize {
                let is_used = bitmap[block / 8] >> (block % 8) & 1 != 0;
                if is_used && group * blocks_per_group as usize + block < blocks_count as usize {
                    ans.push(ChunkInfo {
                        start: block_size as usize * (group * blocks_per_group as usize + block),
                        size: block_size as usize,
                    });
                }
            }
        }
    }

    Ok(Some(ans))
}

fn get_disk_chunks(path: &str) -> Result<Option<Vec<ChunkInfo>>> {
    let disk_size = {
        File::open(path)
            .expect("File cannot be opened")
            .seek(SeekFrom::End(0))
            .expect("failed to seek disk") as usize
    };
    let cfg = GptConfig::new().writable(false);
    let disk = cfg.open(path);
    if disk.is_err() {
        return Ok(None);
    }
    let disk = disk.unwrap();
    let mut pos = 0usize;
    let mut ans = vec![];
    for (id, partition) in disk.partitions().iter().enumerate() {
        let name = format!("{path}p{}", id + 1);
        // lba 512 byte
        let begin = (partition.1.first_lba * 512) as usize;
        let end = ((partition.1.last_lba + 1) * 512) as usize;

        if pos < begin {
            ans.push(ChunkInfo {
                start: pos,
                size: (begin - pos),
            });
        }

        if let Some(chunks) = get_ext4_chunks(&name)? {
            for ChunkInfo { start, size } in chunks {
                ans.push(ChunkInfo {
                    start: start + begin,
                    size,
                });
            }
        } else {
            ans.push(ChunkInfo {
                start: begin,
                size: (end - begin),
            });
        }

        pos = end;
    }

    if pos < disk_size {
        ans.push(ChunkInfo {
            start: pos,
            size: disk_size - pos,
        });
    }

    Ok(Some(ans))
}

fn get_file_chunks(path: &str) -> Result<Vec<ChunkInfo>> {
    let chunks = {
        let disk_chunks = get_disk_chunks(path)?;
        if let Some(chunks) = disk_chunks {
            chunks
        } else {
            let ext4_chunks = get_ext4_chunks(path)?;
            if let Some(chunks) = ext4_chunks {
                chunks
            } else {
                let mut file = File::open(path)?;
                let size = file.seek(SeekFrom::End(0))? as usize;
                let start = 0;
                vec![ChunkInfo { start, size }]
            }
        }
    };

    let mut out = Vec::<ChunkInfo>::new();
    for ChunkInfo { mut start, size } in chunks {
        let end = start + size;

        if let Some(last) = out.last() {
            assert!(last.start + last.size <= start);
            if last.start + last.size == start {
                start = last.start;
                out.pop();
            }
        }

        while start < end {
            out.push(ChunkInfo {
                start,
                size: CHUNK_SIZE.min(end - start),
            });
            start += CHUNK_SIZE;
        }
    }

    Ok(out)
}

pub fn main() -> Result<()> {
    let args = Options::parse();

    ensure!(!args.sources.is_empty(), "Specify at least one source");
    ensure!(!args.destination.is_empty(), "Specify a destination");

    let file_saver: Box<dyn FileSaver> =
        if args.destination.starts_with("http://") || args.destination.starts_with("https://") {
            Box::new(RemoteFileSaver::new(args.destination))
        } else {
            Box::new(LocalFileSaver::new(&args.destination)?)
        };

    let mut stdout = io::stdout().lock();

    let mut info = Vec::new();

    // TODO(veluca): parallelize.
    for s in args.sources {
        let chunks = get_file_chunks(&s)?;

        let total_size: usize = chunks.iter().map(|x| x.size).sum();
        println!("Total size: {}", total_size);

        let mut file = File::open(&s)?;

        let total = chunks.len();

        let chunks: Result<Vec<_>> = chunks
            .into_iter()
            .enumerate()
            .map(|(idx, chnk)| {
                write!(
                    stdout,
                    " pushing chunk {idx} out of {total} from file '{s}'\r"
                )?;
                stdout.flush()?;

                file.seek(SeekFrom::Start(chnk.start as u64))?;
                let mut data = vec![0; chnk.size];
                file.read_exact(&mut data)?;
                let data = bulk::compress(&data, 1)?;
                let hash = file_saver.save_chunk(&data)?;
                Ok(Segment {
                    hash,
                    start: chnk.start,
                    size: chnk.size,
                    csize: data.len(),
                })
            })
            .collect();
        writeln!(stdout)?;

        info.push(pixie_shared::File {
            name: Path::new(&s).to_owned(),
            chunks: chunks?,
        });
    }

    file_saver.save_image(info)
}
