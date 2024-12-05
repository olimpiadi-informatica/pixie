use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use uefi::{
    proto::device_path::DevicePath,
    runtime::{VariableAttributes, VariableVendor},
    CString16,
};

use super::UefiOS;

pub struct BootOptions {
    pub(super) os: UefiOS,
}

impl BootOptions {
    pub fn current(&self) -> u16 {
        let cur = self
            .os
            .get_variable("BootCurrent", &VariableVendor::GLOBAL_VARIABLE)
            .unwrap()
            .0;
        u16::from_le_bytes(cur.try_into().unwrap())
    }

    pub fn order(&self) -> Vec<u16> {
        self.os
            .get_variable("BootOrder", &VariableVendor::GLOBAL_VARIABLE)
            .unwrap()
            .0
            .chunks(2)
            .map(|x| u16::from_le_bytes(x.try_into().unwrap()))
            .collect()
    }

    pub fn set_order(&self, order: &[u16]) {
        let order: Vec<_> = order
            .iter()
            .flat_map(|x| x.to_le_bytes().into_iter())
            .collect();
        self.os
            .set_variable(
                "BootOrder",
                &VariableVendor::GLOBAL_VARIABLE,
                VariableAttributes::NON_VOLATILE
                    | VariableAttributes::BOOTSERVICE_ACCESS
                    | VariableAttributes::RUNTIME_ACCESS,
                &order[..],
            )
            .unwrap()
    }

    pub fn set_next(&self, next: u16) {
        self.os
            .set_variable(
                "BootNext",
                &VariableVendor::GLOBAL_VARIABLE,
                VariableAttributes::NON_VOLATILE
                    | VariableAttributes::BOOTSERVICE_ACCESS
                    | VariableAttributes::RUNTIME_ACCESS,
                &next.to_le_bytes(),
            )
            .unwrap();
    }

    pub fn reboot_target(&self) -> Option<u16> {
        let order = self.order();
        let num_skip = order
            .iter()
            .cloned()
            .position(|x| x == self.current())
            .map(|x| x + 1)
            .unwrap_or(0);
        order.iter().cloned().skip(num_skip).find(|x| *x < 0x2000)
    }

    pub fn get(&self, id: u16) -> Vec<u8> {
        self.os
            .get_variable(&format!("Boot{:04X}", id), &VariableVendor::GLOBAL_VARIABLE)
            .unwrap()
            .0
    }

    pub fn set(&self, id: u16, data: &[u8]) {
        self.os
            .set_variable(
                &format!("Boot{:04X}", id),
                &VariableVendor::GLOBAL_VARIABLE,
                VariableAttributes::NON_VOLATILE
                    | VariableAttributes::BOOTSERVICE_ACCESS
                    | VariableAttributes::RUNTIME_ACCESS,
                data,
            )
            .unwrap();
    }

    /// Boot entry *must* be valid (TODO).
    /// Returns boot option description and device path of the option.
    pub fn boot_entry_info<'a>(&self, entry: &'a [u8]) -> (String, &'a DevicePath) {
        let skip_attropt = &entry[6..];
        let end_of_description = skip_attropt
            .chunks(2)
            .enumerate()
            .find_map(|b| if b.1 == b"\0\0" { Some(b.0) } else { None })
            .expect("Invalid boot entry")
            * 2
            + 2;

        let description = CString16::try_from(
            skip_attropt[..end_of_description]
                .chunks(2)
                .map(|x| u16::from_le_bytes(x.try_into().unwrap()))
                .collect::<Vec<_>>(),
        )
        .unwrap()
        .to_string();

        let device_path =
            unsafe { DevicePath::from_ffi_ptr(skip_attropt[end_of_description..].as_ptr().cast()) };

        (description, device_path)
    }
}
