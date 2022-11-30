use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, Seek, SeekFrom, Write},
    net::{SocketAddrV4, UdpSocket},
    time::Duration,
};

use anyhow::{ensure, Result};
use clap::Parser;
use zstd::bulk;

use pixie_shared::{Segment, BODY_LEN, HEADER_LEN, PACKET_LEN};

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
        let num_packets = (csize + PACKET_LEN - 1) / PACKET_LEN;
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

pub fn main() -> Result<()> {
    let args = Options::parse();

    ensure!(!args.source.is_empty(), "Specify a source");

    let mut stdout = io::stdout().lock();

    let info = fetch_image(args.source)?;

    let mut chunks_info = HashMap::new();

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
            }
            File::options()
                .read(true)
                .write(true)
                .create(true)
                .open(&name)
        })
        .collect::<Result<Vec<_>, _>>()?;

    let total = chunks_info.len();
    write!(stdout, " received 0 chunks out of {}\r", total)?;
    stdout.flush()?;

    // TODO: filter already present chunks from chunks_info

    let socket = UdpSocket::bind(args.listen_on)?;
    socket.set_read_timeout(Some(Duration::from_secs(1)))?;
    let mut buf = [0; 1 << 13];

    let mut received = HashMap::new();

    while !chunks_info.is_empty() {
        match socket.recv_from(&mut buf) {
            Ok((bytes_recv, _)) => {
                assert!(bytes_recv >= 34);
                let hash: &[u8; 32] = buf[..32].try_into().unwrap();
                let index = u16::from_le_bytes(buf[32..34].try_into().unwrap()) as usize;
                let Some(&(size, csize, ref position)) = chunks_info.get(hash) else {
                    continue;
                };

                let pchunk = received
                    .entry(*hash)
                    .or_insert_with(|| PartialChunk::new(csize));

                let rot_index = index.wrapping_add(32);
                let start = rot_index * BODY_LEN;
                pchunk.data[start..start + bytes_recv - HEADER_LEN]
                    .clone_from_slice(&buf[HEADER_LEN..bytes_recv]);

                if !pchunk.missing_first[rot_index] {
                    continue;
                }
                pchunk.missing_first[rot_index] = false;

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

                // TODO: fill lost packets

                let data = bulk::decompress(&pchunk.data[32 * BODY_LEN..], size + 1)?;
                for &(file, offset) in position {
                    files[file].seek(SeekFrom::Start(offset as u64))?;
                    files[file].write_all(&data)?;
                }

                received.remove(hash);
                chunks_info.remove(hash);

                write!(
                    stdout,
                    " received {} chunks out of {}\r",
                    total - chunks_info.len(),
                    total
                )?;
                stdout.flush()?;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                let mut len = 0;
                for (hash, _) in chunks_info.iter() {
                    if len + 32 > PACKET_LEN {
                        break;
                    }
                    buf[len..len + 32].copy_from_slice(hash);
                    len += 32;
                }
                socket.send_to(&buf[..len], args.udp_server)?;
            }
            Err(e) => Err(e)?,
        }
    }

    //  for pixie_shared::File { name, chunks } in info {

    //      let mut seen = HashMap::new();

    //      let total = chunks.len();

    //      let printable_name: &str = &name.to_string_lossy();

    //      for (idx, Segment { hash, start, size }) in chunks.into_iter().enumerate() {
    //          write!(
    //              stdout,
    //              " pulling chunk {idx} out of {total} to file '{printable_name}'\r"
    //          )?;
    //          stdout.flush()?;

    //          let mut data = vec![0; size];

    //          match seen.entry(hash) {
    //              std::collections::hash_map::Entry::Occupied(entry) => {
    //                  let s = *entry.get();
    //                  file.seek(SeekFrom::Start(s as u64))?;
    //                  file.read_exact(&mut data)?;
    //                  file.seek(SeekFrom::Start(start as u64))?;
    //                  file.write_all(&data)?;
    //              }
    //              std::collections::hash_map::Entry::Vacant(entry) => {
    //                  file.seek(SeekFrom::Start(start as u64))?;
    //                  match file.read_exact(&mut data) {
    //                      Ok(()) => {
    //                          let cur_hash = blake3::hash(&data);
    //                          if &hash == cur_hash.as_bytes() {
    //                              continue;
    //                          }
    //                      }
    //                      Err(e) if e.kind() == ErrorKind::UnexpectedEof => {}
    //                      Err(e) => return Err(e.into()),
    //                  }

    //                  let data = file_fetcher.fetch_chunk(hash)?;
    //                  file.seek(SeekFrom::Start(start as u64))?;
    //                  file.write_all(&data)?;

    //                  entry.insert(start);
    //              }
    //          }
    //      }
    //      writeln!(stdout)?;
    //  }

    Ok(())
}
