#![no_main]
#![no_std]
#![feature(negative_impls)]
#![feature(abi_efiapi)]
#![feature(never_type)]
#![deny(unused_must_use)]

use os::UefiOS;

use pixie_shared::Action;
use uefi::prelude::*;

use os::error::Result;

use log::info;

use crate::{pull::pull, push::push, reboot_to_os::reboot_to_os, register::register};

mod os;
mod pull;
mod push;
mod reboot_to_os;
mod register;

#[macro_use]
extern crate alloc;

async fn run(os: UefiOS) -> Result<()> {
    info!("DHCP...");

    os.wait_for_ip().await;

    info!("Connected, IP is {:?}", os.net().ip().unwrap());

    // Local port does not matter.
    let udp = os.udp_bind(None).await?;

    info!("Sending request for command");

    loop {
        udp.send((255, 255, 255, 255), 25640, b"GA").await?;

        let mut command = None;
        // TODO(veluca): add a timeout.
        udp.recv(|data, _ip, _port| {
            command = Some(serde_json::from_slice::<Action>(data));
        })
        .await;

        let command = command.unwrap();

        if let Err(e) = command {
            info!("Invalid action received: {}", e);
        } else {
            match command.unwrap() {
                Action::Wait => {
                    const WAIT_SECS: u64 = 5;
                    info!("Waiting {WAIT_SECS}s for another command...");
                    os.sleep_us(WAIT_SECS * 1_000_000).await;
                    continue;
                }
                Action::Reboot => reboot_to_os(os).await?,
                Action::Register { server, hint_port } => register(os, hint_port, server).await?,
                Action::Push { http_server, image } => push(os, http_server, image).await?,
                Action::Pull {
                    http_server,
                    image,
                    udp_recv_port,
                    udp_server,
                } => pull(os, http_server, image, udp_recv_port, udp_server).await?,
            }
        }
    }
}

#[entry]
fn main(_handle: Handle, system_table: SystemTable<Boot>) -> Status {
    UefiOS::start(system_table, run)
}
