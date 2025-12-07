#![no_main]
#![no_std]
#![deny(unused_must_use)]

#[macro_use]
extern crate alloc;

use alloc::boxed::Box;
use core::net::{Ipv4Addr, SocketAddrV4};

use futures::future::{self, Either};
use pixie_shared::{Action, TcpRequest, UdpRequest, ACTION_PORT, PING_PORT};
use uefi::{entry, Status};

use crate::flash::flash;
use crate::os::error::{Error, Result};
use crate::os::executor::Executor;
use crate::os::net::{TcpStream, UdpSocket, ETH_PACKET_SIZE};
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
            Executor::sleep_us(1_000_000).await;
        })
    };

    let task2 = async {
        let mut buf = [0; ETH_PACKET_SIZE];
        let (data, server) = socket.recv_from(&mut buf).await;
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

async fn shutdown() -> ! {
    #[cfg(feature = "coverage")]
    export_cov::export().await;

    log::info!("Shutting down...");
    Executor::sleep_us(1_000_000).await;
    power_control::shutdown()
}

async fn get_action(stream: &TcpStream) -> Result<Action> {
    let msg = postcard::to_allocvec(&TcpRequest::GetAction)?;
    stream.write_u64_le(msg.len() as u64).await?;
    stream.write_all(&msg).await?;

    let len = stream.read_u64_le().await? as usize;
    let mut buf = vec![0; len];
    stream.read_exact(&mut buf).await?;
    let cmd = postcard::from_bytes(&buf)?;
    Ok(cmd)
}

async fn complete_action(stream: &TcpStream) -> Result<()> {
    let msg = postcard::to_allocvec(&TcpRequest::ActionComplete)?;
    stream.write_u64_le(msg.len() as u64).await?;
    stream.write_all(&msg).await?;

    let len = stream.read_u64_le().await?;
    assert_eq!(len, 0);
    Ok(())
}

async fn run() -> Result<()> {
    let server = server_discover().await?;

    let mut last_was_wait = false;

    Executor::spawn("ping", async move {
        let udp_socket = UdpSocket::bind(None).await.unwrap();
        loop {
            udp_socket
                .send_to(SocketAddrV4::new(*server.ip(), PING_PORT), b"pixie")
                .await
                .unwrap();
            Executor::sleep_us(10_000_000).await;
        }
    });

    loop {
        update_content(|d| d.clear());
        if !last_was_wait {
            log::debug!("Sending request for command");
        }

        let tcp = TcpStream::connect(server).await?;
        let command = get_action(&tcp).await;
        tcp.shutdown().await;
        tcp.force_close().await;

        if let Err(e) = command {
            log::warn!("Error receiving action: {e}");
        } else {
            let command = command.unwrap();
            if matches!(command, Action::Wait) {
                if !last_was_wait {
                    log::warn!("Started waiting for another command...");
                }
                last_was_wait = true;
                const WAIT_10MSECS: u64 = 50;
                for _ in 0..WAIT_10MSECS {
                    Executor::deep_sleep_us(10_000);
                    Executor::sched_yield().await;
                }
            } else {
                last_was_wait = false;
                log::info!("Command: {command:?}");
                match command {
                    Action::Wait => unreachable!(),
                    Action::Boot => power_control::reboot_to_os().await,
                    Action::Restart => {}
                    Action::Shutdown => shutdown().await,
                    Action::Register => register(server).await?,
                    Action::Store => store(server).await?,
                    Action::Flash => flash(server).await?,
                }

                let tcp = TcpStream::connect(server).await?;
                complete_action(&tcp).await?;
                tcp.shutdown().await;
                tcp.force_close().await;

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
