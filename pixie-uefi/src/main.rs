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

    let mut last_was_wait = false;

    loop {
        // Clear any UI drawer.
        os.set_ui_drawer(|_| {});

        if !last_was_wait {
            os.append_message("Sending request for command".into(), MessageKind::Debug);
        }
        let msg = postcard::to_allocvec(&UdpRequest::GetAction)?;
        udp.send((255, 255, 255, 255), pixie_shared::ACTION_PORT, &msg)
            .await?;

        let mut buf = [0; PACKET_SIZE];
        // TODO(veluca): add a timeout.
        let (data, server) = udp.recv(&mut buf).await;
        let command = postcard::from_bytes::<Action>(data);

        if let Err(e) = command {
            os.append_message(format!("Error receiving action: {e}"), MessageKind::Warning);
        } else {
            let command = command.unwrap();
            if matches!(command, Action::Wait) {
                // TODO: consider using tcp
                let msg = postcard::to_allocvec(&UdpRequest::ActionComplete).unwrap();
                udp.send(server.ip, server.port, &msg).await?;
                udp.recv(&mut buf).await;
                if !last_was_wait {
                    os.append_message(
                        format!(
                            "Started waiting for another command at {:.1}s...",
                            os.timer().micros() as f32 * 0.000_001
                        ),
                        MessageKind::Warning,
                    );
                }
                last_was_wait = true;
                const WAIT_10MSECS: u64 = 50;
                for _ in 0..WAIT_10MSECS {
                    os.strong_sleep_us(9_990);
                    os.sleep_us(10).await;
                }
            } else {
                last_was_wait = false;
                os.append_message(format!("Command: {:?}", command), MessageKind::Info);
                match command {
                    Action::Wait => {
                        unreachable!();
                    }
                    Action::Reboot => {
                        let msg = postcard::to_allocvec(&UdpRequest::ActionComplete).unwrap();
                        udp.send(server.ip, server.port, &msg).await?;
                        udp.recv(&mut buf).await;
                        reboot_to_os(os).await;
                    }
                    Action::Register { hint_port } => register(os, server, hint_port).await?,
                    Action::Push { image } => push(os, server, image).await?,
                    Action::Pull { image, chunks_port } => {
                        pull(os, server, image, chunks_port).await?
                    }
                }
            }
            // TODO: consider using tcp
            let msg = postcard::to_allocvec(&UdpRequest::ActionComplete).unwrap();
            udp.send(server.ip, server.port, &msg).await?;
            udp.recv(&mut buf).await;
        }
    }
}

#[entry]
fn main(_handle: Handle, system_table: SystemTable<Boot>) -> Status {
    UefiOS::start(system_table, run)
}
