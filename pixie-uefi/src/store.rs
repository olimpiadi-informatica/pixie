use crate::{
    os::{
        error::{Error, Result},
        mpsc, TcpStream, UefiOS,
    },
    parse_disk, MIN_MEMORY,
};
use alloc::{rc::Rc, vec::Vec};
use core::{cell::RefCell, net::SocketAddrV4};
use lz4_flex::compress;
use pixie_shared::{Chunk, Image, Offset, TcpRequest, UdpRequest, MAX_CHUNK_SIZE};
use uefi::proto::console::text::Color;

#[derive(Debug)]
pub struct ChunkInfo {
    pub start: Offset,
    pub size: usize,
}

async fn save_image(stream: &TcpStream, image: Image) -> Result<()> {
    let req = TcpRequest::UploadImage(image);
    let buf = postcard::to_allocvec(&req)?;
    stream.send_u64_le(buf.len() as u64).await?;
    stream.send(&buf).await?;
    let len = stream.recv_u64_le().await?;
    assert_eq!(len, 0);
    Ok(())
}

enum State {
    ReadingPartitions,
    PushingChunks {
        cur: usize,
        total: usize,
        tsize: usize,
        tcsize: usize,
    },
}

pub async fn store(os: UefiOS, server_address: SocketAddrV4) -> Result<()> {
    let stats = Rc::new(RefCell::new(State::ReadingPartitions));
    let stats2 = stats.clone();
    os.set_ui_drawer(move |os| match &*stats2.borrow() {
        State::ReadingPartitions => {
            os.write_with_color("Reading partitions...", Color::White, Color::Black)
        }
        State::PushingChunks {
            cur,
            total,
            tsize,
            tcsize,
        } => {
            os.write_with_color(
                &format!("Pushed {cur} out of {total} chunks\n"),
                Color::White,
                Color::Black,
            );
            os.write_with_color(
                &format!("total size {tsize}, compressed {tcsize}\n"),
                Color::White,
                Color::Black,
            );
        }
    });

    let bo = os.boot_options();
    let boid = bo.reboot_target().expect("Could not find reboot target");
    let bo_command = bo.get(boid);

    let mut disk = os.open_first_disk();
    let chunks = parse_disk::parse_disk(&mut disk).await?;

    // Split up chunks.
    let mut final_chunks = Vec::<ChunkInfo>::new();
    for ChunkInfo { mut start, size } in chunks {
        let end = start + size;

        if let Some(last) = final_chunks.last() {
            assert!(last.start + last.size <= start);
            if last.start + last.size == start {
                start = last.start;
                final_chunks.pop();
            }
        }

        while start < end {
            let split = (start + 1).next_multiple_of(MAX_CHUNK_SIZE).min(end);
            final_chunks.push(ChunkInfo {
                start,
                size: split - start,
            });
            start = split;
        }
    }

    let udp = os.udp_bind(None).await?;
    let stream_get_csize = os.connect(server_address).await?;
    let stream_upload_chunk = os.connect(server_address).await?;

    let total = final_chunks.len();

    let mut total_size = 0;
    let mut total_csize = 0;

    let total_mem = os.get_total_mem();
    let channel_size =
        (total_mem.saturating_sub(MIN_MEMORY) as usize / (4 * MAX_CHUNK_SIZE)).max(32);
    log::debug!("Total memory: {total_mem}. Channel size: {channel_size}");

    let (tx1, mut rx1) = mpsc::channel(channel_size);
    let (tx2, mut rx2) = mpsc::channel(channel_size);
    let (tx3, mut rx3) = mpsc::channel(channel_size);
    let (tx4, mut rx4) = mpsc::channel(channel_size);

    let task1 = async {
        let mut tx1 = tx1;
        for chunk_info in final_chunks {
            let mut data = vec![0; chunk_info.size];
            disk.read(chunk_info.start as u64, &mut data).await?;
            let cdata = compress(&data);
            let hash = blake3::hash(&data).into();
            let chunk = Chunk {
                hash,
                start: chunk_info.start,
                size: chunk_info.size,
                csize: cdata.len(),
            };
            tx1.send((chunk, cdata)).await;
        }
        Ok::<_, Error>(())
    };

    let task2 = async {
        let mut tx2 = tx2;
        while let Some((chunk, cdata)) = rx1.recv().await {
            let req = TcpRequest::HasChunk(chunk.hash);
            let buf = postcard::to_allocvec(&req)?;
            stream_get_csize.send_u64_le(buf.len() as u64).await?;
            stream_get_csize.send(&buf).await?;
            tx2.send((chunk, cdata)).await;
        }
        Ok(())
    };

    let task3 = async {
        let mut tx3 = tx3;
        while let Some((chunk, cdata)) = rx2.recv().await {
            let len = stream_get_csize.recv_u64_le().await?;
            let mut buf = vec![0; len as usize];
            stream_get_csize.recv(&mut buf).await?;
            let has_chunk: bool = postcard::from_bytes(&buf)?;
            tx3.send((chunk, cdata, has_chunk)).await;
        }
        Ok(())
    };

    let task4 = async {
        let mut tx4 = tx4;
        while let Some((chunk, cdata, has_chunk)) = rx3.recv().await {
            if !has_chunk {
                let req = TcpRequest::UploadChunk(cdata);
                let buf = postcard::to_allocvec(&req)?;
                stream_upload_chunk.send_u64_le(buf.len() as u64).await?;
                stream_upload_chunk.send(&buf).await?;
            }
            tx4.send((chunk, has_chunk)).await;
        }
        Ok(())
    };

    let task5 = async {
        stats.replace(State::PushingChunks {
            cur: 0,
            total,
            tsize: 0,
            tcsize: 0,
        });

        let mut chunks = Vec::new();
        while let Some((chunk, has_chunk)) = rx4.recv().await {
            if !has_chunk {
                let len = stream_upload_chunk.recv_u64_le().await?;
                assert_eq!(len, 0);
            }
            chunks.push(chunk);

            total_size += chunk.size;
            total_csize += chunk.csize;

            stats.replace(State::PushingChunks {
                cur: chunks.len(),
                total,
                tsize: total_size,
                tcsize: total_csize,
            });
            udp.send(
                server_address,
                &postcard::to_allocvec(&UdpRequest::ActionProgress(chunks.len(), total))?,
            )
            .await?;
        }
        Ok(chunks)
    };

    let ((), (), (), (), chunk_hashes) = futures::try_join!(task1, task2, task3, task4, task5)?;

    save_image(
        &stream_upload_chunk,
        Image {
            boot_option_id: boid,
            boot_entry: bo_command,
            disk: chunk_hashes,
        },
    )
    .await?;

    stream_get_csize.close_send().await;
    stream_get_csize.force_close().await;

    stream_upload_chunk.close_send().await;
    // TODO(virv): this could be better
    stream_upload_chunk.force_close().await;

    log::info!("image saved. Total size {total_size}, total csize {total_csize}");

    Ok(())
}
