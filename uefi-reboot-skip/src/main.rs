#![no_main]
#![no_std]
#![feature(abi_efiapi)]

use uefi::{prelude::*, CStr16};

#[entry]
fn main(_handle: Handle, mut system_table: SystemTable<Boot>) -> Status {
    uefi_services::init(&mut system_table).unwrap();

    let services = system_table.runtime_services();

    for var in services.variable_keys().unwrap() {
        let mut varname = arrayvec::ArrayString::<128>::new();
        let name = var.name().unwrap();
        name.as_str_in_buf(&mut varname).unwrap();
        if varname.as_str() == "BootOrder" {
            let mut buf = [0u8; 1024];
            let (data, attrs) = services.get_variable(name, &var.vendor, &mut buf).unwrap();

            let mut bootnext_buf = [0u16; 1024];
            let bootnext = CStr16::from_str_with_buf("BootNext", &mut bootnext_buf).unwrap();

            // Second boot option (bytes 2 and 3, as boot order entries are 16-bit big endian
            // unsigned integers).
            services
                .set_variable(bootnext, &var.vendor, attrs, &data[2..4])
                .unwrap();
        }
    }
    services.reset(uefi::table::runtime::ResetType::Warm, Status::SUCCESS, None);
}
