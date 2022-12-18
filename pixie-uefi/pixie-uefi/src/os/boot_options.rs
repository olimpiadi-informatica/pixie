use alloc::vec::Vec;
use uefi::table::runtime::{VariableAttributes, VariableVendor};

use super::UefiOS;

pub struct BootOptions {
    pub(super) os: UefiOS,
}

impl BootOptions {
    pub fn current(&self) -> u16 {
        u16::from_be_bytes(
            self.os
                .get_variable("BootCurrent", &VariableVendor::GLOBAL_VARIABLE)
                .unwrap()
                .0
                .try_into()
                .unwrap(),
        )
    }

    pub fn order(&self) -> Vec<u16> {
        self.os
            .get_variable("BootOrder", &VariableVendor::GLOBAL_VARIABLE)
            .unwrap()
            .0
            .chunks(2)
            .map(|x| u16::from_be_bytes(x.try_into().unwrap()))
            .collect()
    }

    pub fn set_next(&self, next: u16) {
        self.os
            .set_variable(
                "BootNext",
                &VariableVendor::GLOBAL_VARIABLE,
                VariableAttributes::NON_VOLATILE
                    | VariableAttributes::BOOTSERVICE_ACCESS
                    | VariableAttributes::RUNTIME_ACCESS,
                &next.to_be_bytes(),
            )
            .unwrap();
    }
}
