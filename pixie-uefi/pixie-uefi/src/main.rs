#![no_main]
#![no_std]
#![feature(negative_impls)]
#![feature(abi_efiapi)]

use os::UefiOS;

use uefi::prelude::*;

use os::error::Result;

use log::info;

mod os;

#[macro_use]
extern crate alloc;

async fn run(os: UefiOS) -> Result<()> {
    info!("Started");

    let os2 = os.clone();

    os.spawn(async move {
        os2.sleep_us(2000000).await;
        info!("slept for 2s");
    });

    let conn = os.connect((10, 77, 0, 1), 8000).await.unwrap();
    let req = b"GET /jpeg_xl_data/dices200k-bilevel.zip HTTP/1.1\nHost: old.lucaversari.it\nConnection: close\n\n";
    conn.send(req).await.unwrap();

    let mut read = 0;
    let mut read_buf = [0u8; 1 << 16];
    let start = os.timer().micros();
    let mut last = start;

    let print_speed = |read| {
        let now = os.timer().micros();
        info!(
            "Received {} bytes, {} MB/s",
            read,
            read as f32 / (now - start) as f32,
        );
    };

    loop {
        let r = conn.recv(&mut read_buf).await.unwrap();
        if r == 0 {
            break;
        }
        read += r;
        let now = os.timer().micros();
        if now > last + 1000000 {
            last = now;
            print_speed(read);
        }
    }
    print_speed(read);

    conn.close_send().await;
    conn.wait_until_closed().await;
    info!("Connection closed.");

    Ok(())
}

#[entry]
fn main(_handle: Handle, system_table: SystemTable<Boot>) -> Status {
    UefiOS::start(system_table, run)
}
