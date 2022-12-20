use alloc::{
    boxed::Box,
    collections::BTreeMap,
    string::{String, ToString},
    vec::Vec,
};
use core::mem;

use futures::future::{select, Either};
use log::info;

use miniz_oxide::inflate::decompress_to_vec;

use pixie_shared::{Address, Image, BODY_LEN, HEADER_LEN, PACKET_LEN};

use crate::os::{
    error::{Error, Result},
    HttpMethod, UefiOS, PACKET_SIZE,
};

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

async fn fetch_image(os: UefiOS, server_address: Address, image: &str) -> Result<Image> {
    let resp = os
        .http(
            server_address.ip,
            server_address.port,
            HttpMethod::Get,
            format!("/image/{}", image).as_bytes(),
        )
        .await?;
    Ok(serde_json::from_slice(&resp)?)
}

pub async fn pull(
    os: UefiOS,
    server_address: Address,
    image: String,
    udp_recv_port: u16,
    udp_server: Address,
) -> Result<!> {
    let image = fetch_image(os, server_address, &image).await?;

    let mut chunks_info = BTreeMap::new();
    for chunk in &image.disk {
        chunks_info
            .entry(chunk.hash)
            .or_insert((chunk.size, chunk.csize, Vec::new()))
            .2
            .push(chunk.start);
    }

    let stat_chunks = image.disk.len();
    let stat_unique = chunks_info.len();

    let mut disk = os.open_first_disk();

    for (hash, (size, csize, pos)) in mem::replace(&mut chunks_info, BTreeMap::new()) {
        let mut data = false;
        let mut buf = vec![0; size];
        for &offset in &pos {
            disk.read(offset as u64, &mut buf).await.unwrap();
            if blake3::hash(&buf).as_bytes() == &hash {
                data = true;
                break;
            }
        }
        if data {
            for &offset in &pos {
                disk.write(offset as u64, &buf).await.unwrap();
            }
        } else {
            chunks_info.insert(hash, (size, csize, pos));
        }
    }

    let stat_fetch = chunks_info.len();

    let mut stat_recv = 0usize;
    let mut stat_requested = 0usize;

    info!(
        "Chunks: {} total, {} unique, {} to fetch, {} received, {} requested",
        stat_chunks, stat_unique, stat_fetch, stat_recv, stat_requested
    );

    let socket = os.udp_bind(Some(udp_recv_port)).await?;
    let mut buf = [0; PACKET_SIZE];

    let mut received = BTreeMap::new();

    while !chunks_info.is_empty() {
        let recv = Box::pin(socket.recv(&mut buf));
        let sleep = Box::pin(os.sleep_us(1000_000));
        match select(recv, sleep).await {
            Either::Left(((buf, _addr), _)) => {
                assert!(buf.len() >= 34);
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
                pchunk.data[start..start + buf.len() - HEADER_LEN]
                    .clone_from_slice(&buf[HEADER_LEN..]);

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

                let mut pc = received.remove(hash).unwrap();
                let (_, _, pos) = chunks_info.remove(hash).unwrap();

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

                let data = decompress_to_vec(&pc.data[32 * BODY_LEN..])
                    .map_err(|e| Error::Generic(e.to_string()))?;
                for offset in pos {
                    disk.write(offset as u64, &data).await?;
                }

                stat_recv += 1;
                info!(
                    "Chunks: {} total, {} unique, {} to fetch, {} received, {} requested",
                    stat_chunks, stat_unique, stat_fetch, stat_recv, stat_requested
                );
            }
            Either::Right(((), _sleep)) => {
                let mut buf = [0; PACKET_LEN];
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
                socket
                    .send(udp_server.ip, udp_server.port, &buf[..len])
                    .await?;
                info!(
                    "Chunks: {} total, {} unique, {} to fetch, {} received, {} requested",
                    stat_chunks, stat_unique, stat_fetch, stat_recv, stat_requested
                );
            }
        }
    }

    let bo = os.boot_options();
    let mut order = bo.order();
    order[1] = image.boot_option_id;
    bo.set_order(&order);
    bo.set(image.boot_option_id, &image.boot_entry);

    os.sleep_us(10_000_000).await;
    os.reset();
}

/*

pub fn set_boot_order() -> Result<()> {
    let args = Options::parse();
    ensure!(!args.boot_order_path.is_empty(), "Specify a source");

    let boot_order: BootOrder = serde_json::from_str(&read_to_string(args.boot_order_path)?)?;

    write_boot_option(boot_order.first_option.0, &boot_order.first_option.1)?;
    write_boot_option(boot_order.second_option.0, &boot_order.second_option.1)?;

    let boot_options = current_boot_options()?;
    let opts = [boot_order.first_option.0, boot_order.second_option.0]
        .into_iter()
        .chain(
            boot_options
                .into_iter()
                .filter(|x| *x != boot_order.first_option.0 && *x != boot_order.second_option.0),
        )
        .collect::<Vec<_>>();

    set_boot_options(opts)
}
*/
