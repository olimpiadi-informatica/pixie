#![cfg_attr(target_os = "uefi", no_main)]
#![cfg_attr(target_os = "uefi", no_std)]
#![deny(unused_must_use)]

extern crate alloc;

use alloc::boxed::Box;
use core::net::{Ipv4Addr, SocketAddrV4};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use core::time::Duration;

use futures::future::{self, Either};
use pixie_shared::{ACTION_PORT, Action, PING_PORT, RpcRequest, UdpRequest};
#[cfg(target_os = "uefi")]
use uefi::{Status, entry};

use pixie_client::flash::flash;
use pixie_client::os::error::{Error, Result};
use pixie_client::os::executor::Executor;
use pixie_client::os::net::{ETH_PACKET_SIZE, UdpSocket};
use pixie_client::os::ui::update_content;
use pixie_client::register::register;
use pixie_client::store::store;
use pixie_client::{os, power_control};

static PING_SPAWNED: AtomicBool = AtomicBool::new(false);
static PING_SERVER_IP: AtomicU32 = AtomicU32::new(0);

#[cfg(target_os = "uefi")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    pixie_client::os::panic::handle_panic(info)
}

async fn server_discover() -> Result<SocketAddrV4> {
    let socket = UdpSocket::bind(None).await?;

    let task1 = async {
        let msg = postcard::to_allocvec(&UdpRequest::Discover)?;
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
            if data.is_empty() && server.port() == ACTION_PORT {
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
    pixie_client::export_cov::export().await;

    log::info!("Shutting down...");
    Executor::sleep(Duration::from_secs(1)).await;
    power_control::shutdown()
}

async fn run() -> Result<()> {
    let server = server_discover().await?;

    let mut last_was_wait = false;

    PING_SERVER_IP.store(server.ip().to_bits(), Ordering::Relaxed);
    if !PING_SPAWNED.swap(true, Ordering::Relaxed) {
        Executor::spawn("ping", async {
            let udp_socket = loop {
                if let Ok(sock) = UdpSocket::bind(None).await {
                    break sock;
                }
                Executor::sleep(Duration::from_secs(1)).await;
            };
            loop {
                let ip = Ipv4Addr::from_bits(PING_SERVER_IP.load(Ordering::Relaxed));
                let _ = udp_socket
                    .send_to(SocketAddrV4::new(ip, PING_PORT), b"pixie")
                    .await;
                Executor::sleep(Duration::from_secs(10)).await;
            }
        });
    }

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

#[cfg(target_os = "uefi")]
#[entry]
fn main() -> Status {
    os::start(run)
}

#[cfg(not(target_os = "uefi"))]
fn main() {
    os::start(run)
}
