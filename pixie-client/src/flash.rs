use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::fmt::Write;
use core::mem;
use core::net::SocketAddrV4;
use core::time::Duration;

use futures::future::{Either, select};
use log::info;
use lz4_flex::decompress;
use pixie_shared::chunk_codec::Decoder;
use pixie_shared::util::BytesFmt;
use pixie_shared::{CHUNKS_PORT, ChunkHash, Image, MAX_CHUNK_SIZE, RpcRequest, UdpRequest};

use crate::MIN_MEMORY;
use crate::os::boot_options::BootOptions;
use crate::os::error::{Error, Result};
use crate::os::executor::Executor;
use crate::os::net::{ETH_PACKET_SIZE, UdpSocket};
use crate::os::timer::Timer;
use crate::os::ui::{DrawArea, update_content};
use crate::os::{disk, memory};

async fn fetch_image(server_addr: SocketAddrV4) -> Result<Image> {
    let socket = UdpSocket::bind(None).await?;
    let mut buf = Vec::new();
    loop {
        let (total_len, part): (u32, Vec<u8>) = socket
            .rpc(
                server_addr,
                RpcRequest::GetImage {
                    offset: buf.len() as u32,
                },
            )
            .await?;
        buf.extend_from_slice(&part);
        if buf.len() >= total_len as usize {
            break;
        }
    }
    Ok(postcard::from_bytes(&buf)?)
}

#[derive(Clone, PartialEq, Eq)]
struct Stats {
    chunks: usize,
    unique: usize,
    fetch: usize,
    recv: usize,
    pack_recv: usize,
    requested: usize,
}

pub fn handle_packet(
    buf: &[u8],
    chunks_info: &mut BTreeMap<ChunkHash, (usize, usize, Vec<usize>)>,
    received: &mut BTreeMap<ChunkHash, Decoder>,
    last_seen: &mut Vec<ChunkHash>,
) -> Result<Option<(Vec<usize>, Vec<u8>)>> {
    if buf.len() < 34 {
        return Ok(None);
    }
    let hash: ChunkHash = buf[..32].try_into().unwrap();
    let &(size, csize, _) = match chunks_info.get(&hash) {
        Some(info) => info,
        None => return Ok(None),
    };

    let decoder = received.entry(hash).or_insert_with(|| Decoder::new(csize));
    if last_seen.last() != Some(&hash) {
        last_seen.retain(|x| x != &hash);
        last_seen.push(hash);
    }

    if let Err(e) = decoder.add_packet(&buf[32..]) {
        log::warn!("Received invalid packet for chunk {hash:02x?}: {e}");
        return Ok(None);
    }
    let Some(cdata) = decoder.finish() else {
        return Ok(None);
    };

    received.remove(&hash);
    last_seen.retain(|x| x != &hash);

    let data = match decompress(&cdata, size) {
        Ok(d) if d.len() == size && blake3::hash(&d).as_bytes() == &hash => d,
        Ok(d) => {
            log::warn!(
                "Decompressed chunk {hash:02x?} size/hash mismatch (got {} bytes, expected {size}); re-fetching",
                d.len()
            );
            return Ok(None);
        }
        Err(e) => {
            log::warn!("Failed to decompress chunk {hash:02x?}: {e}; re-fetching");
            return Ok(None);
        }
    };

    let (_, _, pos) = chunks_info.remove(&hash).unwrap();

    Ok(Some((pos, data)))
}

pub async fn flash(server_addr: SocketAddrV4) -> Result<()> {
    let image = fetch_image(server_addr).await?;

    let mut chunks_info = BTreeMap::new();
    for chunk in &image.disk {
        chunks_info
            .entry(chunk.hash)
            .or_insert((chunk.size, chunk.csize, Vec::new()))
            .2
            .push(chunk.start);
    }

    info!("Obtained chunks; {} distinct chunks", chunks_info.len());

    let stats = RefCell::new(Stats {
        chunks: image.disk.len(),
        unique: chunks_info.len(),
        fetch: 0,
        recv: 0,
        pack_recv: 0,
        requested: 0,
    });

    let draw = |draw_area: &mut DrawArea| {
        draw_area.clear();
        writeln!(draw_area, "{} total chunks", stats.borrow().chunks).unwrap();
        writeln!(draw_area, "{} unique chunks", stats.borrow().unique).unwrap();
        writeln!(draw_area, "{} chunks to fetch", stats.borrow().fetch).unwrap();
        writeln!(draw_area, "{} chunks received", stats.borrow().recv).unwrap();
        writeln!(draw_area, "{} packets received", stats.borrow().pack_recv).unwrap();
        writeln!(draw_area, "{} chunks requested", stats.borrow().requested).unwrap();
    };

    update_content(draw);

    let done = Cell::new(false);

    let draw_task = async {
        let mut last_stats = stats.borrow().clone();
        while !done.get() {
            Executor::sleep(Duration::from_millis(100)).await;
            let stats = stats.borrow().clone();
            if stats == last_stats {
                continue;
            }
            update_content(draw);
            last_stats = stats;
        }
        Ok(())
    };

    let mut disk = disk::Disk::largest().await;

    let mut scan_buf = vec![0u8; MAX_CHUNK_SIZE];
    let mut last_draw_us = Timer::micros();
    for (scanned, (hash, (size, csize, pos))) in mem::take(&mut chunks_info).into_iter().enumerate()
    {
        if size > scan_buf.len() {
            scan_buf.resize(size, 0);
        }
        let buf = &mut scan_buf[..size];
        let mut found = None;
        for &offset in &pos {
            disk.read(offset as u64, buf).await?;
            if blake3::hash(buf).as_bytes() == &hash {
                found = Some(offset);
                break;
            }
        }
        if let Some(found) = found {
            for &offset in &pos {
                if offset != found {
                    disk.write(offset as u64, buf).await?;
                }
            }
        } else {
            chunks_info.insert(hash, (size, csize, pos));
            stats.borrow_mut().fetch = chunks_info.len();
        }
        let now_us = Timer::micros();
        if now_us.saturating_sub(last_draw_us) >= 100_000 || (scanned & 63) == 63 {
            update_content(draw);
            last_draw_us = now_us;
        }
    }
    drop(scan_buf);
    update_content(draw);

    info!("Disk scanned; {} chunks to fetch", stats.borrow().fetch);

    let socket = UdpSocket::bind(Some(CHUNKS_PORT)).await?;
    let mut buf = [0; ETH_PACKET_SIZE];

    let mut received = BTreeMap::new();

    let free_mem = memory::stats().free;
    let channel_size =
        (free_mem.saturating_sub(MIN_MEMORY) as usize / (4 * MAX_CHUNK_SIZE)).clamp(32, 128);
    let max_chunks =
        (free_mem.saturating_sub(MIN_MEMORY) as usize / (2 * MAX_CHUNK_SIZE)).clamp(64, 128);
    log::debug!(
        "Free memory: {}. Channel size: {channel_size}, max chunks in memory: {max_chunks}",
        BytesFmt(free_mem)
    );

    let (tx, rx) = thingbuf::mpsc::channel(channel_size);

    let task1 = async {
        let tx = tx;
        let mut last_seen = Vec::new();
        while !chunks_info.is_empty() {
            let recv = Box::pin(socket.recv_from(&mut buf));
            let sleep = Box::pin(Executor::sleep(Duration::from_millis(100)));
            match select(recv, sleep).await {
                Either::Left(((buf, _addr), _)) => {
                    stats.borrow_mut().pack_recv += 1;
                    if buf.len() < 34 {
                        continue;
                    }

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
                    let chunks: Vec<_> =
                        chunks_info.iter().take(40).map(|(hash, _)| *hash).collect();
                    stats.borrow_mut().requested += chunks.len();
                    let msg = postcard::to_allocvec(&UdpRequest::RequestChunks(chunks))?;
                    socket.send_to(server_addr, &msg).await?;
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
                .send_to(server_addr, &postcard::to_allocvec(&msg)?)
                .await?;
        }
        done.set(true);
        Ok(())
    };

    let ((), (), ()) = futures::try_join!(task1, task2, draw_task)?;

    disk.flush().await?;

    info!("Fetch complete, updating boot options");

    BootOptions::set(image.boot_option_id, &image.boot_entry);

    let mut order = BootOptions::order();
    if let Some(target) = BootOptions::reboot_target() {
        let mut replaced = false;
        for x in &mut order {
            if *x == target {
                *x = image.boot_option_id;
                replaced = true;
            }
        }
        if !replaced && !order.contains(&image.boot_option_id) {
            order.push(image.boot_option_id);
        }
    } else if !order.contains(&image.boot_option_id) {
        order.push(image.boot_option_id);
    }
    let mut deduped = Vec::with_capacity(order.len());
    for id in order {
        if !deduped.contains(&id) {
            deduped.push(id);
        }
    }
    BootOptions::set_order(&deduped);

    Ok(())
}
