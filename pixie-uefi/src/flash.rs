use crate::os::{
    error::{Error, Result},
    mpsc, TcpStream, UefiOS, PACKET_SIZE,
};
use alloc::{boxed::Box, collections::BTreeMap, rc::Rc, string::ToString, vec::Vec};
use core::{cell::RefCell, mem, net::SocketAddrV4};
use futures::future::{select, Either};
use log::info;
use lz4_flex::decompress;
use pixie_shared::{Image, TcpRequest, UdpRequest, BODY_LEN, CHUNKS_PORT, HEADER_LEN};
use uefi::proto::console::text::Color;

struct PartialChunk {
    data: Vec<u8>,
    missing_first: Vec<bool>,
    missing_second: [u16; 32],
    missing_third: u16,
}

impl PartialChunk {
    fn new(csize: usize) -> Self {
        let num_packets = csize.div_ceil(BODY_LEN);
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

async fn fetch_image(stream: &TcpStream) -> Result<Image> {
    let req = TcpRequest::GetImage;
    let mut buf = postcard::to_allocvec(&req)?;
    stream.send_u64_le(buf.len() as u64).await?;
    stream.send(&buf).await?;
    let len = stream.recv_u64_le().await?;
    buf.resize(len as usize, 0);
    stream.recv_exact(&mut buf).await?;
    Ok(postcard::from_bytes(&buf)?)
}

struct Stats {
    chunks: usize,
    unique: usize,
    fetch: usize,
    recv: usize,
    pack_recv: usize,
    requested: usize,
}

async fn handle_packet(
    buf: &[u8],
    chunks_info: &mut BTreeMap<[u8; 32], (usize, usize, Vec<usize>)>,
    received: &mut BTreeMap<[u8; 32], PartialChunk>,
) -> Result<Option<(Vec<usize>, Vec<u8>)>> {
    let hash: &[u8; 32] = buf[..32].try_into().unwrap();
    let index = u16::from_le_bytes(buf[32..34].try_into().unwrap()) as usize;
    let csize = match chunks_info.get(hash) {
        Some(&(_, csize, _)) => csize,
        _ => return Ok(None),
    };

    let pchunk = received
        .entry(*hash)
        .or_insert_with(|| PartialChunk::new(csize));

    let rot_index = (index as u16).wrapping_add(32) as usize;
    match &mut pchunk.missing_first[rot_index] {
        false => return Ok(None),
        x @ true => *x = false,
    }

    let start = rot_index * BODY_LEN;
    pchunk.data[start..start + buf.len() - HEADER_LEN].clone_from_slice(&buf[HEADER_LEN..]);

    let group = index & 31;
    match &mut pchunk.missing_second[group] {
        0 => return Ok(None),
        x @ 1 => *x = 0,
        x @ 2.. => {
            *x -= 1;
            return Ok(None);
        }
    }

    match &mut pchunk.missing_third {
        0 => unreachable!(),
        x @ 1 => *x = 0,
        x @ 2.. => {
            *x -= 1;
            return Ok(None);
        }
    }

    let (size, _, pos) = chunks_info.remove(hash).unwrap();
    let mut pchunk = received.remove(hash).unwrap();

    let mut xor = [[0; BODY_LEN]; 32];
    for packet in 0..pchunk.missing_first.len() {
        if !pchunk.missing_first[packet] {
            let group = packet & 31;
            pchunk.data[BODY_LEN * packet..]
                .iter()
                .zip(xor[group].iter_mut())
                .for_each(|(a, b)| *b ^= a);
        }
    }
    for packet in 0..pchunk.missing_first.len() {
        if pchunk.missing_first[packet] {
            let group = packet & 31;
            pchunk.data[BODY_LEN * packet..]
                .iter_mut()
                .zip(xor[group].iter())
                .for_each(|(a, b)| *a = *b);
        }
    }

    let data = decompress(&pchunk.data[32 * BODY_LEN..], size)
        .map_err(|e| Error::Generic(e.to_string()))?;

    Ok(Some((pos, data)))
}

pub async fn flash(os: UefiOS, server_addr: SocketAddrV4) -> Result<()> {
    let stream = os.connect(server_addr).await?;
    let image = fetch_image(&stream).await?;
    stream.close_send().await;
    // TODO(virv): this could be better
    stream.force_close().await;

    let mut chunks_info = BTreeMap::new();
    for chunk in &image.disk {
        chunks_info
            .entry(chunk.hash)
            .or_insert((chunk.size, chunk.csize, Vec::new()))
            .2
            .push(chunk.start);
    }

    info!("Obtained chunks; {} distinct chunks", chunks_info.len());

    let stats = Rc::new(RefCell::new(Stats {
        chunks: image.disk.len(),
        unique: chunks_info.len(),
        fetch: 0,
        recv: 0,
        pack_recv: 0,
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
            &format!("{} packets received\n", stats2.borrow().pack_recv),
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
        let mut found = None;
        let mut buf = vec![0; size];
        for &offset in &pos {
            disk.read(offset as u64, &mut buf).await.unwrap();
            let cdata = lz4_flex::compress(&buf);
            if blake3::hash(&cdata).as_bytes() == &hash {
                found = Some(offset);
                break;
            }
        }
        if let Some(found) = found {
            for &offset in &pos {
                if offset != found {
                    disk.write(offset as u64, &buf).await.unwrap();
                }
            }
        } else {
            chunks_info.insert(hash, (size, csize, pos));
            stats.borrow_mut().fetch = chunks_info.len();
        }
    }

    info!("Disk scanned; {} chunks to fetch", stats.borrow().fetch);

    let socket = os.udp_bind(Some(CHUNKS_PORT)).await?;
    let mut buf = [0; PACKET_SIZE];

    let mut received = BTreeMap::new();

    let (tx, mut rx) = mpsc::channel(128);

    let task1 = async {
        let mut tx = tx;
        while !chunks_info.is_empty() {
            let recv = Box::pin(socket.recv(&mut buf));
            let sleep = Box::pin(os.sleep_us(100_000));
            match select(recv, sleep).await {
                Either::Left(((buf, _addr), _)) => {
                    stats.borrow_mut().pack_recv += 1;
                    assert!(buf.len() >= 34);

                    let chunk = handle_packet(buf, &mut chunks_info, &mut received).await?;
                    if let Some((pos, data)) = chunk {
                        tx.send((pos, data)).await;
                    }

                    if received.len() > 128 {
                        received.pop_last();
                    }
                }
                Either::Right(((), _sleep)) => {
                    // TODO(virv): compute the number of chunks to request
                    let chunks: Vec<_> =
                        chunks_info.iter().take(40).map(|(hash, _)| *hash).collect();
                    stats.borrow_mut().requested += chunks.len();
                    let msg = postcard::to_allocvec(&UdpRequest::RequestChunks(chunks)).unwrap();
                    socket.send(server_addr, &msg).await?;
                }
            }
        }
        Ok::<_, Error>(())
    };

    let task2 = async {
        while let Some((pos, data)) = rx.recv().await {
            for offset in pos {
                disk.write(offset as u64, &data).await?;
            }

            stats.borrow_mut().recv += 1;

            let msg = UdpRequest::ActionProgress(stats.borrow().recv, stats.borrow().fetch);
            socket
                .send(server_addr, &postcard::to_allocvec(&msg)?)
                .await?;
        }
        Ok(())
    };

    let ((), ()) = futures::try_join!(task1, task2)?;

    info!("Fetch complete, updating boot options");

    let bo = os.boot_options();
    let mut order = bo.order();
    let reboot_target = bo.reboot_target();
    if let Some(target) = reboot_target {
        order = order
            .into_iter()
            .map(|x| if x != target { x } else { image.boot_option_id })
            .collect();
    } else {
        order.push(image.boot_option_id);
    };
    bo.set_order(&order);
    bo.set(image.boot_option_id, &image.boot_entry);

    Ok(())
}
