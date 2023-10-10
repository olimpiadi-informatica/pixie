use alloc::{rc::Rc, string::String, vec::Vec};
use core::cell::RefCell;

use lz4_flex::compress;
use uefi::proto::console::text::Color;

mod parse_disk;

use crate::os::{
    error::{Error, Result},
    mpsc, MessageKind, TcpStream, UefiOS,
};
use pixie_shared::{Address, Chunk, Image, Offset, TcpRequest, UdpRequest, CHUNK_SIZE};

#[derive(Debug)]
struct ChunkInfo {
    start: Offset,
    size: usize,
}

async fn save_image(stream: &TcpStream, name: String, image: Image) -> Result<()> {
    let req = TcpRequest::UploadImage(name, image);
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

pub async fn push(os: UefiOS, server_address: Address, image: String) -> Result<()> {
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
                &format!("Pushed {} out of {} chunks\n", cur, total),
                Color::White,
                Color::Black,
            );
            os.write_with_color(
                &format!("total size {}, compressed {}\n", tsize, tcsize),
                Color::White,
                Color::Black,
            );
        }
    });

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
            let split = end.min((start / CHUNK_SIZE + 1) * CHUNK_SIZE);
            final_chunks.push(ChunkInfo {
                start,
                size: split - start,
            });
            start = split;
        }
    }

    let udp = os.udp_bind(None).await?;
    let stream_get_csize = os.connect(server_address.ip, server_address.port).await?;
    let stream_upload_chunk = os.connect(server_address.ip, server_address.port).await?;

    let total = final_chunks.len();

    let mut total_size = 0;
    let mut total_csize = 0;

    let (tx1, mut rx1) = mpsc::channel(32);
    let (tx2, mut rx2) = mpsc::channel(32);
    let (tx3, mut rx3) = mpsc::channel(32);
    let (tx4, mut rx4) = mpsc::channel(32);
    let (tx5, mut rx5) = mpsc::channel(32);

    let task1 = async {
        let mut tx1 = tx1;
        for chnk in final_chunks {
            let mut data = vec![0; chnk.size];
            disk.read(chnk.start as u64, &mut data).await?;
            let hash = blake3::hash(&data).into();
            tx1.send((chnk.start, hash, data)).await;
        }
        Ok::<_, Error>(())
    };

    let task2 = async {
        let mut tx2 = tx2;
        while let Some((start, hash, data)) = rx1.recv().await {
            let req = TcpRequest::GetChunkSize(hash);
            let buf = postcard::to_allocvec(&req)?;
            stream_get_csize.send_u64_le(buf.len() as u64).await?;
            stream_get_csize.send(&buf).await?;
            tx2.send((start, hash, data)).await;
        }
        Ok(())
    };

    let task3 = async {
        let mut tx3 = tx3;
        while let Some((start, hash, data)) = rx2.recv().await {
            let len = stream_get_csize.recv_u64_le().await?;
            let mut buf = vec![0; len as usize];
            stream_get_csize.recv(&mut buf).await?;
            let csize: Option<usize> = postcard::from_bytes(&buf)?;
            tx3.send((start, hash, data, csize)).await;
        }
        Ok(())
    };

    let task4 = async {
        let mut tx4 = tx4;
        while let Some((start, hash, data, csize)) = rx3.recv().await {
            let (csize, cdata) = match csize {
                Some(csize) => (csize, None),
                None => {
                    let cdata = compress(&data);
                    (cdata.len(), Some(cdata))
                }
            };
            tx4.send((start, hash, data, csize, cdata)).await;
        }
        Ok(())
    };

    let task5 = async {
        let mut tx5 = tx5;
        while let Some((start, hash, data, csize, cdata)) = rx4.recv().await {
            let check_ack = cdata.is_some();
            if let Some(cdata) = cdata {
                let req = TcpRequest::UploadChunk(hash, cdata);
                let buf = postcard::to_allocvec(&req)?;
                stream_upload_chunk.send_u64_le(buf.len() as u64).await?;
                stream_upload_chunk.send(&buf).await?;
            }
            tx5.send((start, hash, data, csize, check_ack)).await;
        }
        Ok(())
    };

    let task6 = async {
        stats.replace(State::PushingChunks {
            cur: 0,
            total,
            tsize: 0,
            tcsize: 0,
        });

        let mut chunks = Vec::new();
        while let Some((start, hash, data, csize, check_ack)) = rx5.recv().await {
            if check_ack {
                let len = stream_upload_chunk.recv_u64_le().await?;
                assert_eq!(len, 0);
            }
            chunks.push(Chunk {
                hash,
                start,
                size: data.len(),
                csize,
            });

            total_size += data.len();
            total_csize += csize;

            stats.replace(State::PushingChunks {
                cur: chunks.len(),
                total,
                tsize: total_size,
                tcsize: total_csize,
            });
            udp.send(
                server_address.ip,
                server_address.port,
                &postcard::to_allocvec(&UdpRequest::ActionProgress(chunks.len(), total))?,
            )
            .await?;
        }
        Ok(chunks)
    };

    let ((), (), (), (), (), chunk_hashes) =
        futures::try_join!(task1, task2, task3, task4, task5, task6)?;

    let bo = os.boot_options();
    let boid = bo.order()[1];
    let bo_command = bo.get(boid);

    save_image(
        &stream_upload_chunk,
        image.clone(),
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

    os.append_message(
        format!(
            "image saved at {:?}/{}. Total size {total_size}, total csize {total_csize}",
            server_address, image,
        ),
        MessageKind::Info,
    );

    Ok(())
}
