use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;
use core::net::SocketAddrV4;

use log::info;
use lz4_flex::compress;
use pixie_shared::util::BytesFmt;
use pixie_shared::{Chunk, Image, Offset, TcpRequest, UdpRequest, MAX_CHUNK_SIZE};
use uefi::proto::console::text::Color;

use crate::os::boot_options::BootOptions;
use crate::os::error::{Error, Result};
use crate::os::net::{TcpStream, UdpSocket};
use crate::os::{disk, memory, UefiOS};
use crate::{parse_disk, MIN_MEMORY};

#[derive(Debug)]
pub struct ChunkInfo {
    pub start: Offset,
    pub size: usize,
}

async fn save_image(stream: &TcpStream, image: Image) -> Result<()> {
    let req = TcpRequest::UploadImage(image);
    let buf = postcard::to_allocvec(&req)?;
    stream.write_u64_le(buf.len() as u64).await?;
    stream.write_all(&buf).await?;
    let len = stream.read_u64_le().await?;
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

    let boid = BootOptions::reboot_target().expect("Could not find reboot target");
    let bo_command = BootOptions::get(boid);

    let mut disk = disk::Disk::largest();
    let chunks = parse_disk::parse_disk(&mut disk).await?;
    info!(
        "Total size of chunks: {}",
        BytesFmt(chunks.iter().map(|x| x.size as u64).sum::<u64>())
    );

    let udp = UdpSocket::bind(None).await?;
    let stream_get_csize = TcpStream::connect(server_address).await?;
    let stream_upload_chunk = TcpStream::connect(server_address).await?;

    let total = chunks.len();

    let mut total_size = 0;
    let mut total_csize = 0;

    let free_mem = memory::stats().free;
    let channel_size =
        (free_mem.saturating_sub(MIN_MEMORY) as usize / (4 * MAX_CHUNK_SIZE)).max(32);
    log::debug!(
        "Free memory: {}. Channel size: {channel_size}",
        BytesFmt(free_mem)
    );

    let (tx1, rx1) = thingbuf::mpsc::channel(channel_size);
    let (tx2, rx2) = thingbuf::mpsc::channel(channel_size);
    let (tx3, rx3) = thingbuf::mpsc::channel(channel_size);
    let (tx4, rx4) = thingbuf::mpsc::channel(channel_size);

    let task1 = async {
        let tx1 = tx1;
        for chunk_info in chunks {
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
            tx1.send((chunk, cdata)).await.expect("receiver dropped");
        }
        Ok::<_, Error>(())
    };

    let task2 = async {
        let tx2 = tx2;
        while let Some((chunk, cdata)) = rx1.recv().await {
            let req = TcpRequest::HasChunk(chunk.hash);
            let buf = postcard::to_allocvec(&req)?;
            stream_get_csize.write_u64_le(buf.len() as u64).await?;
            stream_get_csize.write_all(&buf).await?;
            tx2.send((chunk, cdata)).await.expect("receiver dropped");
        }
        Ok(())
    };

    let task3 = async {
        let tx3 = tx3;
        while let Some((chunk, cdata)) = rx2.recv().await {
            let len = stream_get_csize.read_u64_le().await?;
            let mut buf = vec![0; len as usize];
            stream_get_csize.read_exact(&mut buf).await?;
            let has_chunk: bool = postcard::from_bytes(&buf)?;
            tx3.send((chunk, cdata, has_chunk))
                .await
                .expect("receiver dropped");
        }
        Ok(())
    };

    let task4 = async {
        let tx4 = tx4;
        while let Some((chunk, cdata, has_chunk)) = rx3.recv().await {
            if !has_chunk {
                let req = TcpRequest::UploadChunk(cdata);
                let buf = postcard::to_allocvec(&req)?;
                stream_upload_chunk.write_u64_le(buf.len() as u64).await?;
                stream_upload_chunk.write_all(&buf).await?;
            }
            tx4.send((chunk, has_chunk))
                .await
                .expect("receiver dropped");
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
                let len = stream_upload_chunk.read_u64_le().await?;
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
            udp.send_to(
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
            boot_entry: bo_command.to_vec(),
            disk: chunk_hashes,
        },
    )
    .await?;

    stream_get_csize.shutdown().await;
    stream_get_csize.force_close().await;

    stream_upload_chunk.shutdown().await;
    // TODO(virv): this could be better
    stream_upload_chunk.force_close().await;

    log::info!(
        "image saved. Total size {}, total csize {}",
        BytesFmt(total_size as u64),
        BytesFmt(total_csize as u64)
    );

    Ok(())
}
