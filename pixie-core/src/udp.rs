use std::{collections::BTreeSet, fmt::Write, fs, net::SocketAddrV4, ops::Bound, path::Path};

use tokio::{
    net::UdpSocket,
    time::{self, Duration, Instant},
};

use anyhow::{ensure, Result};
use serde::Deserialize;

use pixie_shared::{BODY_LEN, PACKET_LEN};

#[derive(Debug, Deserialize)]
pub struct Config {
    listen_on: SocketAddrV4,
    dest_addr: SocketAddrV4,
    bits_per_second: u32,
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::new();
    for byte in bytes {
        write!(s, "{:02x}", byte).unwrap();
    }
    s
}

pub async fn main(storage_dir: &Path, config: Config) -> Result<()> {
    let socket = UdpSocket::bind(config.listen_on).await?;
    socket.set_broadcast(true)?;
    let mut queue = BTreeSet::<[u8; 32]>::new();
    let mut write_buf = [0; PACKET_LEN];
    let mut wait_for = Instant::now();
    let mut index = [0; 32];

    loop {
        let mut read_buf = [0; PACKET_LEN];
        let (len, _) = socket.recv_from(&mut read_buf).await?;
        // TODO
        assert!(len % 32 == 0);
        for i in 0..len / 32 {
            queue.insert(read_buf[32 * i..32 * i + 32].try_into().unwrap());
        }

        wait_for = wait_for.max(Instant::now());
        while let Some(hash) = queue
            .range((Bound::Excluded(index), Bound::Unbounded))
            .next()
            .or_else(|| queue.iter().next())
        {
            index = *hash;
            queue.remove(&index);

            let filename = storage_dir.join("chunks").join(to_hex(&index));
            let data = fs::read(&filename)?;

            let num_packets = (data.len() + BODY_LEN - 1) / BODY_LEN;
            write_buf[..32].clone_from_slice(&index);

            let mut xor = [[0; BODY_LEN]; 32];

            for index in 0..num_packets {
                write_buf[32..34].clone_from_slice(&(index as u16).to_le_bytes());
                let start = index * BODY_LEN;
                let len = BODY_LEN.min(data.len() - start);
                let body = &data[start..start + len];
                let group = index & 31;
                body.iter()
                    .zip(xor[group].iter_mut())
                    .for_each(|(a, b)| *b ^= a);
                write_buf[34..34 + len].clone_from_slice(body);

                time::sleep_until(wait_for).await;
                let sent_len = socket
                    .send_to(&write_buf[..34 + len], config.dest_addr)
                    .await?;
                ensure!(sent_len == 34 + len, "Could not send packet");
                wait_for += 8 * (sent_len as u32) * Duration::from_secs(1) / config.bits_per_second;
            }

            for index in 0..32.min(num_packets) {
                write_buf[32..34].clone_from_slice(&(index as u16).wrapping_sub(32).to_le_bytes());
                let len = BODY_LEN;
                let body = &xor[index];
                write_buf[34..34 + len].clone_from_slice(body);

                time::sleep_until(wait_for).await;
                let sent_len = socket
                    .send_to(&write_buf[..34 + len], config.dest_addr)
                    .await?;
                ensure!(sent_len == 34 + len, "Could not send packet");
                wait_for += 8 * (sent_len as u32) * Duration::from_secs(1) / config.bits_per_second;
            }
        }
    }
}
