#![no_main]
#![no_std]
#![feature(negative_impls)]
#![feature(abi_efiapi)]

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
    info!("Started");

    // Local port does not matter.
    let udp = os.udp_bind(None).await?;

    udp.send((255, 255, 255, 255), 25640, b"GA").await?;

    loop {
        let mut command = None;

        udp.recv(|data, ip, port| {
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
                }
                Action::Reboot => reboot_to_os(os).await,
                Action::Register { server } => register(os, server).await,
                Action::Push { http_server, path } => push(os, http_server, path).await,
                Action::Pull {
                    http_server,
                    path,
                    udp_recv_port,
                    udp_server,
                } => pull(os, http_server, path, udp_recv_port, udp_server).await,
            }
        }
    }
}

#[entry]
fn main(_handle: Handle, system_table: SystemTable<Boot>) -> Status {
    UefiOS::start(system_table, run)
}
