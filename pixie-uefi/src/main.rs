#![no_main]
#![no_std]
#![deny(unused_must_use)]

#[macro_use]
extern crate alloc;

use alloc::boxed::Box;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::time::Duration;

use futures::future::{self, Either};
use pixie_shared::{ACTION_PORT, Action, PING_PORT, RpcRequest, UdpRequest};
use uefi::{Status, entry};

use crate::flash::flash;
use crate::os::error::{Error, Result};
use crate::os::executor::Executor;
use crate::os::net::{ETH_PACKET_SIZE, UdpSocket};
use crate::os::ui::update_content;
use crate::register::register;
use crate::store::store;

mod flash;
mod os;
mod parse_disk;
mod power_control;
mod register;
mod store;

#[cfg(feature = "coverage")]
mod export_cov;

// Memory to keep free for non-chunk storage.
const MIN_MEMORY: u64 = 32 << 20;

async fn server_discover() -> Result<SocketAddrV4> {
    let socket = UdpSocket::bind(None).await?;

    let task1 = async {
        let msg = postcard::to_allocvec(&UdpRequest::Discover).unwrap();
        #[allow(unreachable_code)]
        Ok::<_, Error>(loop {
            socket
                .send_to(SocketAddrV4::new(Ipv4Addr::BROADCAST, ACTION_PORT), &msg)
                .await?;
            Executor::sleep(Duration::from_secs(1)).await;
        })
    };

    let task2 = async {
        let mut buf = [0; ETH_PACKET_SIZE];
        loop {
            let (data, server) = socket.recv_from(&mut buf).await;
            if data.is_empty() {
                break Ok::<_, Error>(server);
            }
        }
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

async fn shutdown() -> ! {
    #[cfg(feature = "coverage")]
    export_cov::export().await;

    log::info!("Shutting down...");
    Executor::sleep(Duration::from_secs(1)).await;
    power_control::shutdown()
}

async fn run() -> Result<()> {
    let server = server_discover().await?;

    let mut last_was_wait = false;

    Executor::spawn("ping", async move {
        if let Ok(udp_socket) = UdpSocket::bind(None).await {
            loop {
                let _ = udp_socket
                    .send_to(SocketAddrV4::new(*server.ip(), PING_PORT), b"pixie")
                    .await;
                Executor::sleep(Duration::from_secs(10)).await;
            }
        }
    });

    let rpc_socket = UdpSocket::bind(None).await?;

    loop {
        update_content(|d| d.clear());
        if !last_was_wait {
            log::debug!("Sending request for command");
        }

        let command: Result<Action> = rpc_socket.rpc(server, RpcRequest::GetAction).await;

        if let Err(e) = command {
            log::warn!("Error receiving action: {e}");
        } else {
            let command = command.unwrap();
            if matches!(command, Action::Wait) {
                if !last_was_wait {
                    log::warn!("Started waiting for another command...");
                }
                last_was_wait = true;
                Executor::sleep(Duration::from_millis(500)).await;
            } else {
                last_was_wait = false;
                log::info!("Command: {command:?}");
                match command {
                    Action::Wait => unreachable!(),
                    Action::Boot => power_control::reboot_to_os().await,
                    Action::Restart => {}
                    Action::Shutdown => shutdown().await,
                    Action::Register => {
                        Executor::spawn("register", register(server)).join().await?
                    }
                    Action::Store => Executor::spawn("store", store(server)).join().await?,
                    Action::Flash => Executor::spawn("flash", flash(server)).join().await?,
                }

                rpc_socket
                    .rpc::<()>(server, RpcRequest::ActionComplete)
                    .await?;

                if command == Action::Restart {
                    power_control::reset()
                }
            }
        }
    }
}

#[entry]
fn main() -> Status {
    os::start(run)
}
