#![no_main]
#![no_std]
#![feature(negative_impls)]
#![feature(never_type)]
#![deny(unused_must_use)]

#[macro_use]
extern crate alloc;

use crate::{
    flash::flash,
    os::{
        error::{Error, Result},
        MessageKind, TcpStream, UefiOS, PACKET_SIZE,
    },
    reboot_to_os::reboot_to_os,
    register::register,
    store::store,
};
use alloc::boxed::Box;
use core::net::{Ipv4Addr, SocketAddrV4};
use futures::future::{self, Either};
use pixie_shared::{Action, TcpRequest, UdpRequest, ACTION_PORT, PING_PORT};
use uefi::prelude::*;

mod flash;
mod os;
mod parse_disk;
mod reboot_to_os;
mod register;
mod store;

async fn server_discover(os: UefiOS) -> Result<SocketAddrV4> {
    let socket = os.udp_bind(None).await?;

    let task1 = async {
        // TODO(virv): there must be a better way...
        if false {
            return Err::<!, _>(Error::Generic("".into()));
        }

        let msg = postcard::to_allocvec(&UdpRequest::Discover).unwrap();
        loop {
            socket
                .send(SocketAddrV4::new(Ipv4Addr::BROADCAST, ACTION_PORT), &msg)
                .await?;
            os.sleep_us(1_000_000).await;
        }
    };

    let task2 = async {
        let mut buf = [0; PACKET_SIZE];
        let (data, server) = socket.recv(&mut buf).await;
        assert_eq!(data.len(), 0);
        Ok::<_, Error>(server)
    };

    let x = future::try_select(Box::pin(task1), Box::pin(task2)).await;
    let server = match x {
        Ok(Either::Left((never, _))) => never,
        Ok(Either::Right((server, _))) => server,
        Err(Either::Left((e, _))) => Err(e)?,
        Err(Either::Right((e, _))) => Err(e)?,
    };

    Ok(server)
}

async fn get_action(stream: &TcpStream) -> Result<Action> {
    let msg = postcard::to_allocvec(&TcpRequest::GetAction)?;
    stream.send_u64_le(msg.len() as u64).await?;
    stream.send(&msg).await?;

    let len = stream.recv_u64_le().await? as usize;
    let mut buf = vec![0; len];
    stream.recv_exact(&mut buf).await?;
    let cmd = postcard::from_bytes(&buf)?;
    Ok(cmd)
}

async fn complete_action(stream: &TcpStream) -> Result<()> {
    let msg = postcard::to_allocvec(&TcpRequest::ActionComplete)?;
    stream.send_u64_le(msg.len() as u64).await?;
    stream.send(&msg).await?;

    let len = stream.recv_u64_le().await?;
    assert_eq!(len, 0);
    Ok(())
}

async fn run(os: UefiOS) -> Result<!> {
    let server = server_discover(os).await?;

    let mut last_was_wait = false;

    os.spawn("ping", async move {
        let udp_socket = os.udp_bind(None).await.unwrap();
        loop {
            udp_socket
                .send(SocketAddrV4::new(*server.ip(), PING_PORT), b"pixie")
                .await
                .unwrap();
            os.sleep_us(10_000_000).await;
        }
    });

    loop {
        // Clear any UI drawer.
        os.set_ui_drawer(|_| {});

        if !last_was_wait {
            os.append_message("Sending request for command".into(), MessageKind::Debug);
        }

        let tcp = os.connect(server).await?;
        let command = get_action(&tcp).await;
        tcp.close_send().await;
        tcp.force_close().await;

        if let Err(e) = command {
            os.append_message(format!("Error receiving action: {e}"), MessageKind::Warning);
        } else {
            let command = command.unwrap();
            if matches!(command, Action::Wait) {
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
                    os.deep_sleep_us(10_000);
                    os.schedule().await;
                }
            } else {
                last_was_wait = false;
                os.append_message(format!("Command: {:?}", command), MessageKind::Info);
                match command {
                    Action::Wait => unreachable!(),
                    Action::Reboot => reboot_to_os(os).await,
                    Action::Register => register(os, server).await?,
                    Action::Store => store(os, server).await?,
                    Action::Flash => flash(os, server).await?,
                }

                let tcp = os.connect(server).await?;
                complete_action(&tcp).await?;
                tcp.close_send().await;
                tcp.force_close().await;
            }
        }
    }
}

#[entry]
fn main() -> Status {
    UefiOS::start(run)
}
