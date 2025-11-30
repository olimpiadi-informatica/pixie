use crate::{
    os::{
        error::{Error, Result},
        TcpStream, UefiOS, PACKET_SIZE,
    },
    MIN_MEMORY,
};
use alloc::{boxed::Box, collections::BTreeMap, rc::Rc, string::ToString, vec::Vec};
use core::{cell::RefCell, mem, net::SocketAddrV4};
use futures::future::{select, Either};
use log::info;
use lz4_flex::decompress;
use pixie_shared::{
    chunk_codec::Decoder, util::BytesFmt, ChunkHash, Image, TcpRequest, UdpRequest, CHUNKS_PORT,
    MAX_CHUNK_SIZE,
};
use uefi::proto::console::text::Color;

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

fn handle_packet(
    buf: &[u8],
    chunks_info: &mut BTreeMap<ChunkHash, (usize, usize, Vec<usize>)>,
    received: &mut BTreeMap<ChunkHash, Decoder>,
    last_seen: &mut Vec<ChunkHash>,
) -> Result<Option<(Vec<usize>, Vec<u8>)>> {
    let hash: ChunkHash = buf[..32].try_into().unwrap();
    let csize = match chunks_info.get(&hash) {
        Some(&(_, csize, _)) => csize,
        _ => return Ok(None),
    };

    let decoder = received.entry(hash).or_insert_with(|| Decoder::new(csize));
    last_seen.retain(|x| x != &hash);
    last_seen.push(hash);

    if let Err(e) = decoder.add_packet(&buf[32..]) {
        log::warn!("Received invalid packet for chunk {hash:02x?}: {e}");
        return Ok(None);
    }
    let Some(cdata) = decoder.finish() else {
        return Ok(None);
    };

    let (size, _, pos) = chunks_info.remove(&hash).unwrap();
    received.remove(&hash).unwrap();
    last_seen.retain(|x| x != &hash);

    let data = decompress(&cdata, size).map_err(|e| Error::Generic(e.to_string()))?;
    assert_eq!(data.len(), size);

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
            if blake3::hash(&buf).as_bytes() == &hash {
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

    let (tx, rx) = thingbuf::mpsc::channel(128);

    let task1 = async {
        let tx = tx;
        let mut last_seen = Vec::new();
        let total_mem = os.get_total_mem();
        let max_chunks = (total_mem.saturating_sub(MIN_MEMORY) as usize / MAX_CHUNK_SIZE).max(128);
        log::debug!(
            "Total memory: {}. Max chunks in memory: {max_chunks}",
            BytesFmt(total_mem)
        );
        while !chunks_info.is_empty() {
            let recv = Box::pin(socket.recv(&mut buf));
            let sleep = Box::pin(os.sleep_us(100_000));
            match select(recv, sleep).await {
                Either::Left(((buf, _addr), _)) => {
                    stats.borrow_mut().pack_recv += 1;
                    assert!(buf.len() >= 34);

                    let chunk =
                        handle_packet(buf, &mut chunks_info, &mut received, &mut last_seen)?;
                    if let Some((pos, data)) = chunk {
                        tx.send((pos, data)).await.expect("receiver was dropped");
                    }

                    assert_eq!(last_seen.len(), received.len());
                    if last_seen.len() > max_chunks {
                        let hash = last_seen.remove(0);
                        received
                            .remove(&hash)
                            .expect("last_seen should contain only received chunks");
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
