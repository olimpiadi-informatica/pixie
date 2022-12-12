#![no_main]
#![no_std]
#![feature(negative_impls)]
#![feature(abi_efiapi)]

use os::UefiOS;

use uefi::prelude::*;

use uefi::Result;

use log::info;

mod os;

#[macro_use]
extern crate alloc;

async fn run(os: UefiOS) -> Result {
    info!("Started");

    os.spawn(async {
        info!("task started");
    });

    /*
    start_timer(os);

    let handles = services.locate_handle_buffer(uefi::table::boot::SearchType::AllHandles)?;

    let mut protos = vec![];

    for h in handles.handles() {
        for p in services.protocols_per_handle(*h)?.protocols() {
            protos.push(**p);
        }
    }

    protos.sort_by_key(|x| format!("{x}"));
    protos.dedup();
    for p in protos.iter() {
        info!("{p}");
        services.stall(1000000);
    }

    services.stall(10000000);

    let req = b"GET /jpeg_xl_data/dices200k-bilevel.zip HTTP/1.1\nHost: old.lucaversari.it\nConnection: close\n\n";

    let mut net = NetworkInterface::new(os);

    while !net.has_ip() {
        net.poll();
    }

    info!("DHCP done");

    let tcp = net
        .connect(smoltcp::wire::IpEndpoint {
            addr: smoltcp::wire::IpAddress::Ipv4(smoltcp::wire::Ipv4Address::new(10, 77, 0, 1)),
            port: 8000,
        })
        .unwrap();

    loop {
        net.poll();
        match net.tcp_state(&tcp) {
            State::Established => break,
            State::Closed => {
                panic!("Failed to connect")
            }
            _ => {}
        }
    }

    info!("Connected");

    let mut pos = 0;
    while pos < req.len() {
        let sent = net.send_tcp(&tcp, &req[pos..]);
        info!("{} state: {}", sent, net.tcp_state(&tcp));
        pos += sent;
        net.poll();
    }

    info!("Request sent");

    let mut read = 0;
    let mut read_buf = [0u8; 1 << 16];
    let start = get_time_micros();
    let mut last = start;
    loop {
        let r = net.recv_tcp(&tcp, &mut read_buf);
        if r.is_none() {
            break;
        }
        read += r.unwrap();
        net.poll();
        let now = get_time_micros();
        if now > last + 1000000 {
            last = now;
            info!(
                "Received {} bytes, {} MB/s, state: {}",
                read,
                read as f32 / (now - start) as f32,
                net.tcp_state(&tcp)
            );
        }
    }

    let now = get_time_micros();
    info!(
        "Received {} bytes, {} MB/s",
        read,
        read as f32 / (now - start) as f32
    );

    loop {
        net.poll();
        match net.tcp_state(&tcp) {
            State::Closed => {
                net.remove(tcp);
                break;
            }
            _ => {}
        }
    }

    loop {
        net.poll();
    }
    */

    Ok(())
}

#[entry]
fn main(_handle: Handle, system_table: SystemTable<Boot>) -> Status {
    UefiOS::start(system_table, run)
}
