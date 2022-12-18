use log::info;
use uefi::table::runtime::VariableVendor;

use crate::os::{error::Result, UefiOS};

pub async fn reboot_to_os(os: UefiOS) -> Result<!> {
    let (boot_order, attrs) = os.get_variable("BootOrder", &VariableVendor::GLOBAL_VARIABLE)?;
    // Second boot option (bytes 2 and 3, as boot order entries are 16-bit big endian
    // unsigned integers).
    os.set_variable(
        "BootNext",
        &VariableVendor::GLOBAL_VARIABLE,
        attrs,
        &boot_order[2..4],
    )?;
    os.reset();
}
