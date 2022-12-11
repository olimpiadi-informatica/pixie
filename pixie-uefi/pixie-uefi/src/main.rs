#![no_main]
#![no_std]
#![feature(abi_efiapi)]

use os::net::NetworkInterface;
use os::timer::start_timer;
use os::UefiOS;
use uefi::prelude::*;

use uefi::Result;

mod os;

#[macro_use]
extern crate alloc;

fn dump_services(os: UefiOS) -> Result {
    start_timer(os);

    /*
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
    */

    let mut net = NetworkInterface::new(os);

    loop {
        net.poll();
    }

    Ok(())
}

#[entry]
fn main(_handle: Handle, system_table: SystemTable<Boot>) -> Status {
    let os = UefiOS::new(system_table);

    dump_services(os).unwrap();

    os.reset();
}
