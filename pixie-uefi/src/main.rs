#![no_main]
#![no_std]
#![feature(negative_impls)]
#![feature(abi_efiapi)]
#![feature(never_type)]
#![deny(unused_must_use)]

use os::{MessageKind, UefiOS};

use pixie_shared::{Action, UdpRequest};
use uefi::prelude::*;

use os::{error::Result, PACKET_SIZE};

use crate::{pull::pull, push::push, reboot_to_os::reboot_to_os, register::register};

mod os;
mod pull;
mod push;
mod reboot_to_os;
mod register;

#[macro_use]
extern crate alloc;

async fn run(os: UefiOS) -> Result<()> {
    // Local port does not matter.
    let udp = os.udp_bind(None).await?;

    loop {
        // Clear any UI drawer.
        os.set_ui_drawer(|_| {});

        os.append_message("Sending request for command".into(), MessageKind::Info);
        let msg = serde_json::to_vec(&UdpRequest::GetAction).unwrap();
        udp.send((255, 255, 255, 255), pixie_shared::ACTION_PORT, &msg)
            .await?;

        let mut buf = [0; PACKET_SIZE];
        // TODO(veluca): add a timeout.
        let (data, server) = udp.recv(&mut buf).await;
        let command = serde_json::from_slice::<Action>(data);

        if let Err(e) = command {
            os.append_message(format!("Error receiving action: {e}"), MessageKind::Warning);
        } else {
            let command = command.unwrap();
            os.append_message(format!("Command: {:?}", command), MessageKind::Info);
            match command {
                Action::Wait => {
                    const WAIT_SECS: u64 = 5;
                    os.append_message(
                        format!("Waiting {WAIT_SECS}s for another command..."),
                        MessageKind::Warning,
                    );
                    for _ in 0..WAIT_SECS * 10 {
                        os.strong_sleep_us(99_990);
                        os.sleep_us(10).await;
                    }
                }
                Action::Reboot => {
                    let msg = serde_json::to_vec(&UdpRequest::ActionComplete).unwrap();
                    udp.send(server.ip, server.port, &msg).await?;
                    udp.recv(&mut buf).await;
                    reboot_to_os(os).await;
                }
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

        // TODO: consider using tcp
        let msg = serde_json::to_vec(&UdpRequest::ActionComplete).unwrap();
        udp.send(server.ip, server.port, &msg).await?;
        udp.recv(&mut buf).await;
    }
}

#[entry]
fn main(_handle: Handle, system_table: SystemTable<Boot>) -> Status {
    UefiOS::start(system_table, run)
}
