#![no_main]
#![no_std]
#![feature(negative_impls)]
#![feature(abi_efiapi)]

use alloc::{borrow::ToOwned, string::String};
use os::UefiOS;

use uefi::prelude::*;

use os::error::Result;

use log::info;

mod os;

#[macro_use]
extern crate alloc;

async fn run(os: UefiOS) -> Result<()> {
    info!("Started");

    // Local port does not matter.
    let udp = os.udp_bind(None).await?;

    udp.send((255, 255, 255, 255), 25640, b"GA").await?;

    udp.recv(|data, ip, port| {
        info!(
            "Received {} from {:?}:{port}",
            String::from_utf8(data.to_owned()).unwrap(),
            ip
        );
    })
    .await;

    Ok(())
}

#[entry]
fn main(_handle: Handle, system_table: SystemTable<Boot>) -> Status {
    UefiOS::start(system_table, run)
}
