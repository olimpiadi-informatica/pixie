use alloc::{
    boxed::Box,
    collections::BTreeMap,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::{cell::RefCell, mem};
use uefi::proto::console::text::Color;

use futures::future::{select, Either};

use miniz_oxide::inflate::decompress_to_vec;

use pixie_shared::{Address, Image, TcpRequest, UdpRequest, BODY_LEN, HEADER_LEN};

use crate::os::{
    error::{Error, Result},
    TcpStream, UefiOS, PACKET_SIZE,
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

async fn fetch_image(stream: &TcpStream, image: String) -> Result<Image> {
    let req = TcpRequest::GetImage(image);
    let mut buf = serde_json::to_vec(&req)?;
    stream.send_u64_le(buf.len() as u64).await?;
    stream.send(&buf).await?;
    let len = stream.recv_u64_le().await?;
    buf.resize(len as usize, 0);
    stream.recv_exact(&mut buf).await?;
    Ok(serde_json::from_slice(&buf)?)
}

struct Stats {
    chunks: usize,
    unique: usize,
    fetch: usize,
    recv: usize,
    requested: usize,
}

pub async fn pull(
    os: UefiOS,
    server_address: Address,
    image: String,
    udp_recv_port: u16,
    udp_server: Address,
    progress_address: Address,
) -> Result<()> {
    let stream = os.connect(server_address.ip, server_address.port).await?;
    let image = fetch_image(&stream, image).await?;
    stream.close_send().await;
    stream.wait_until_closed().await;

    let mut chunks_info = BTreeMap::new();
    for chunk in &image.disk {
        chunks_info
            .entry(chunk.hash)
            .or_insert((chunk.size, chunk.csize, Vec::new()))
            .2
            .push(chunk.start);
    }

    let stats = Arc::new(RefCell::new(Stats {
        chunks: image.disk.len(),
        unique: chunks_info.len(),
        fetch: 0,
        recv: 0,
        requested: 0,
    }));

    let stats2 = stats.clone();
    os.set_ui_drawer(move |os| {
        os.write_with_color(
            &format!("{} total chunks\n", stats2.borrow().chunks),
            Color::White,
            Color::Black,
        );
        os.write_with_color(
            &format!("{} unique chunks\n", stats2.borrow().unique),
            Color::White,
            Color::Black,
        );
        os.write_with_color(
            &format!("{} chunks to fetch\n", stats2.borrow().fetch),
            Color::White,
            Color::Black,
        );
        os.write_with_color(
            &format!("{} chunks received\n", stats2.borrow().recv),
            Color::White,
            Color::Black,
        );
        os.write_with_color(
            &format!("{} chunks requested\n", stats2.borrow().requested),
            Color::White,
            Color::Black,
        );
    });

    let mut disk = os.open_first_disk();

    for (hash, (size, csize, pos)) in mem::take(&mut chunks_info) {
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

    stats.borrow_mut().fetch = chunks_info.len();

    let socket = os.udp_bind(Some(udp_recv_port)).await?;
    let mut buf = [0; PACKET_SIZE];

    let mut received = BTreeMap::new();

    while !chunks_info.is_empty() {
        let recv = Box::pin(socket.recv(&mut buf));
        let sleep = Box::pin(os.sleep_us(1_000_000));
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

                stats.borrow_mut().recv += 1;

                socket
                    .send(
                        progress_address.ip,
                        progress_address.port,
                        &serde_json::to_vec(&UdpRequest::ActionProgress(
                            stats.borrow().recv,
                            stats.borrow().fetch,
                        ))?,
                    )
                    .await?;
            }
            Either::Right(((), _sleep)) => {
                // TODO(virv): compute the number of chunks to request
                let chunks: Vec<_> = chunks_info.iter().take(10).map(|(hash, _)| *hash).collect();
                stats.borrow_mut().requested += chunks.len();
                let msg = serde_json::to_vec(&UdpRequest::RequestChunks(chunks)).unwrap();
                socket.send(udp_server.ip, udp_server.port, &msg).await?;
            }
        }
    }

    let bo = os.boot_options();
    let mut order = bo.order();
    order.retain(|&x| x != image.boot_option_id);
    order.insert(1, image.boot_option_id);
    bo.set_order(&order);
    bo.set(image.boot_option_id, &image.boot_entry);

    Ok(())
}
