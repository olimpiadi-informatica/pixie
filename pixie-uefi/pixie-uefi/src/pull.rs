use alloc::string::String;
use pixie_shared::Address;

use crate::os::UefiOS;

pub async fn pull(
    os: UefiOS,
    server_address: Address,
    server_path: String,
    udp_recv_port: u16,
    udp_server: Address,
) -> ! {
    todo!();
}

/*
use std::{
    collections::HashMap,
    fs::{self, File as StdFile},
    io::{self, BufWriter, Read, Seek, SeekFrom, Write},
    net::{SocketAddrV4, UdpSocket},
    sync::Arc,
    time::Duration,
};

use tokio::{
    fs::File,
    io::{AsyncSeekExt, AsyncWriteExt},
    sync::Mutex,
};

use anyhow::{ensure, Result};
use clap::Parser;
use zstd::bulk;

use pixie_shared::{Segment, BODY_LEN, CHUNK_SIZE, HEADER_LEN, PACKET_LEN};

#[derive(Parser, Debug)]
struct Options {
    #[clap(short, long, value_parser)]
    source: String,
    #[clap(short, long, value_parser)]
    listen_on: SocketAddrV4,
    #[clap(short, long, value_parser)]
    udp_server: SocketAddrV4,
}

fn fetch_image(url: String) -> Result<Vec<pixie_shared::File>> {
    let resp = reqwest::blocking::get(&url)?;
    ensure!(
        resp.status().is_success(),
        "failed to fetch image: status ({}) is not success",
        resp.status().as_u16()
    );
    let body = resp.text()?;
    let files = serde_json::from_str(&body)?;
    Ok(files)
}

struct PartialChunk {
    data: Vec<u8>,
    missing_first: Vec<bool>,
    missing_second: [u16; 32],
    missing_third: u16,
}

impl PartialChunk {
    fn new(csize: usize) -> Self {
        let num_packets = (csize + BODY_LEN - 1) / BODY_LEN;
        let data = vec![0; 32 * BODY_LEN + csize];
        let missing_first = vec![true; 32 + num_packets];
        let missing_second: [u16; 32] = (0..32)
            .map(|i| ((num_packets + 31 - i) / 32) as u16)
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();
        let missing_third = missing_second.iter().map(|&x| (x != 0) as u16).sum();
        PartialChunk {
            data,
            missing_first,
            missing_second,
            missing_third,
        }
    }
}

async fn save_chunk(
    mut pc: PartialChunk,
    pos: Vec<(usize, usize)>,
    files: Arc<[Mutex<File>]>,
) -> Result<()> {
    let mut xor = [[0; BODY_LEN]; 32];
    for packet in 0..pc.missing_first.len() {
        if !pc.missing_first[packet] {
            let group = packet & 31;
            pc.data[BODY_LEN * packet..]
                .iter()
                .zip(xor[group].iter_mut())
                .for_each(|(a, b)| *b ^= a);
        }
    }
    for packet in 0..pc.missing_first.len() {
        if pc.missing_first[packet] {
            let group = packet & 31;
            pc.data[BODY_LEN * packet..]
                .iter_mut()
                .zip(xor[group].iter())
                .for_each(|(a, b)| *a = *b);
        }
    }

    let data = bulk::decompress(&pc.data[32 * BODY_LEN..], CHUNK_SIZE + 1)?;
    for (file, offset) in pos {
        let mut lock = files[file].lock().await;
        lock.seek(SeekFrom::Start(offset as u64)).await?;
        lock.write_all(&data).await?;
    }
    Ok(())
}

#[tokio::main]
pub async fn main() -> Result<()> {
    let args = Options::parse();

    ensure!(!args.source.is_empty(), "Specify a source");

    let stdout = io::stdout().lock();
    let mut stdout = BufWriter::new(stdout);

    let info = fetch_image(args.source)?;

    let mut chunks_info = HashMap::new();

    let mut stat_chunks = 0usize;

    let mut files = info
        .into_iter()
        .enumerate()
        .map(|(idx, pixie_shared::File { name, chunks })| {
            if let Some(prefix) = name.parent() {
                fs::create_dir_all(prefix)?;
            }

            for Segment {
                hash,
                start,
                size,
                csize,
            } in chunks
            {
                chunks_info
                    .entry(hash)
                    .or_insert((size, csize, Vec::new()))
                    .2
                    .push((idx, start));
                stat_chunks += 1;
            }

            Ok(StdFile::options()
                .read(true)
                .write(true)
                .create(true)
                .open(&name)?)
        })
        .collect::<Result<Vec<_>>>()?;

    let stat_unique = chunks_info.len();
    write!(stdout, "Total chunks:     {stat_chunks}\n")?;
    write!(stdout, "Unique chunks:    {stat_unique}\n")?;

    chunks_info.retain(|hash, &mut (size, _, ref pos)| {
        let mut data = false;
        let mut buf = vec![0; size];
        for &(file, offset) in pos {
            files[file].seek(SeekFrom::Start(offset as u64)).unwrap();
            match files[file].read_exact(&mut buf) {
                Ok(()) => {
                    if blake3::hash(&bulk::compress(&buf, 1).unwrap()).as_bytes() == hash {
                        data = true;
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => (),
                Err(e) => Err(e).unwrap(),
            }
        }
        if data {
            for &(file, offset) in pos {
                files[file].seek(SeekFrom::Start(offset as u64)).unwrap();
                files[file].write_all(&buf).unwrap();
            }
        }
        !data
    });

    let files: Arc<[_]> = files
        .into_iter()
        .map(|f| Mutex::new(File::from_std(f)))
        .collect();

    let stat_fetch = chunks_info.len();
    write!(stdout, "Chunks to fetch:  {stat_fetch}\n")?;

    let mut stat_recv = 0usize;
    let mut stat_requested = 0usize;
    write!(stdout, "Chunks received:  {stat_recv}\n")?;
    write!(stdout, "Chunks requested: {stat_requested}\n")?;
    stdout.flush()?;

    let socket = UdpSocket::bind(args.listen_on)?;
    socket.set_read_timeout(Some(Duration::from_secs(1)))?;
    let mut buf = [0; 1 << 13];

    let mut received = HashMap::new();

    let mut tasks = Vec::new();

    while !chunks_info.is_empty() {
        match socket.recv_from(&mut buf) {
            Ok((bytes_recv, _)) => {
                assert!(bytes_recv >= 34);
                let hash: &[u8; 32] = buf[..32].try_into().unwrap();
                let index = u16::from_le_bytes(buf[32..34].try_into().unwrap()) as usize;
                let csize = match chunks_info.get(hash) {
                    Some(&(_, csize, _)) => csize,
                    _ => continue,
                };

                let pchunk = received
                    .entry(*hash)
                    .or_insert_with(|| PartialChunk::new(csize));

                let rot_index = (index as u16).wrapping_add(32) as usize;
                match &mut pchunk.missing_first[rot_index] {
                    false => continue,
                    x @ true => *x = false,
                }

                let start = rot_index * BODY_LEN;
                pchunk.data[start..start + bytes_recv - HEADER_LEN]
                    .clone_from_slice(&buf[HEADER_LEN..bytes_recv]);

                let group = index & 31;
                match &mut pchunk.missing_second[group] {
                    0 => continue,
                    x @ 1 => *x = 0,
                    x @ 2.. => {
                        *x -= 1;
                        continue;
                    }
                }

                match &mut pchunk.missing_third {
                    0 => unreachable!(),
                    x @ 1 => *x = 0,
                    x @ 2.. => {
                        *x -= 1;
                        continue;
                    }
                }

                let pc = received.remove(hash).unwrap();
                let (_, _, pos) = chunks_info.remove(hash).unwrap();

                let task = tokio::spawn(save_chunk(pc, pos, files.clone()));
                tasks.push(task);

                stat_recv += 1;

                write!(stdout, "\x1b[2A")?;
                write!(stdout, "Chunks received:  {stat_recv}\n")?;
                write!(stdout, "Chunks requested: {stat_requested}\n")?;
                stdout.flush()?;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                let mut len = 2;
                buf[..2].copy_from_slice(b"RB");
                for (hash, _) in chunks_info.iter() {
                    if len + 32 > PACKET_LEN {
                        break;
                    }
                    buf[len..len + 32].copy_from_slice(hash);
                    len += 32;
                    stat_requested += 1;
                }
                socket.send_to(&buf[..len], args.udp_server)?;

                write!(stdout, "\x1b[2A")?;
                write!(stdout, "Chunks received:  {stat_recv}\n")?;
                write!(stdout, "Chunks requested: {stat_requested}\n")?;
                stdout.flush()?;
            }
            Err(e) => Err(e)?,
        }
    }

    for task in tasks {
        task.await??;
    }

    Ok(())
}
*/
