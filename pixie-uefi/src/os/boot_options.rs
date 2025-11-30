use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use uefi::proto::device_path::DevicePath;
use uefi::runtime::{VariableAttributes, VariableVendor};
use uefi::{CStr16, CString16};

use crate::os::error::{Error, Result};

#[derive(Debug)]
pub struct Variable {
    name: CString16,
    vendor: VariableVendor,
}

impl Variable {
    pub fn new(name: &str, vendor: VariableVendor) -> Self {
        // name.len() should be enough, but...
        let mut name_buf = vec![0u16; name.len() * 2 + 16];
        let name = CStr16::from_str_with_buf(name, &mut name_buf).unwrap();
        Self {
            name: name.into(),
            vendor,
        }
    }

    pub fn get(&self) -> Result<(Box<[u8]>, VariableAttributes)> {
        uefi::runtime::get_variable_boxed(&self.name, &self.vendor)
            .map_err(|e| Error(format!("Error getting variable: {e:?}")))
    }

    pub fn set(&self, data: &[u8], attrs: VariableAttributes) -> Result<()> {
        uefi::runtime::set_variable(&self.name, &self.vendor, attrs, data)
            .map_err(|e| Error(format!("Error setting variable: {e:?}")))
    }
}

pub struct BootOptions;

impl BootOptions {
    pub fn current() -> u16 {
        let cur = Variable::new("BootCurrent", VariableVendor::GLOBAL_VARIABLE)
            .get()
            .unwrap()
            .0;
        u16::from_le_bytes((*cur).try_into().unwrap())
    }

    pub fn order() -> Vec<u16> {
        Variable::new("BootOrder", VariableVendor::GLOBAL_VARIABLE)
            .get()
            .unwrap()
            .0
            .chunks(2)
            .map(|x| u16::from_le_bytes(x.try_into().unwrap()))
            .collect()
    }

    pub fn set_order(order: &[u16]) {
        let order: Vec<_> = order
            .iter()
            .flat_map(|x| x.to_le_bytes().into_iter())
            .collect();
        Variable::new("BootOrder", VariableVendor::GLOBAL_VARIABLE)
            .set(
                &order[..],
                VariableAttributes::NON_VOLATILE
                    | VariableAttributes::BOOTSERVICE_ACCESS
                    | VariableAttributes::RUNTIME_ACCESS,
            )
            .unwrap()
    }

    pub fn set_next(next: u16) {
        Variable::new("BootNext", VariableVendor::GLOBAL_VARIABLE)
            .set(
                &next.to_le_bytes(),
                VariableAttributes::NON_VOLATILE
                    | VariableAttributes::BOOTSERVICE_ACCESS
                    | VariableAttributes::RUNTIME_ACCESS,
            )
            .unwrap();
    }

    pub fn reboot_target() -> Option<u16> {
        let order = Self::order();
        let num_skip = order
            .iter()
            .cloned()
            .position(|x| x == Self::current())
            .map(|x| x + 1)
            .unwrap_or(0);
        order.iter().cloned().skip(num_skip).find(|x| *x < 0x2000)
    }

    pub fn get(id: u16) -> Box<[u8]> {
        Variable::new(&format!("Boot{id:04X}"), VariableVendor::GLOBAL_VARIABLE)
            .get()
            .unwrap()
            .0
    }

    pub fn set(id: u16, data: &[u8]) {
        Variable::new(&format!("Boot{id:04X}"), VariableVendor::GLOBAL_VARIABLE)
            .set(
                data,
                VariableAttributes::NON_VOLATILE
                    | VariableAttributes::BOOTSERVICE_ACCESS
                    | VariableAttributes::RUNTIME_ACCESS,
            )
            .unwrap();
    }

    /// Boot entry *must* be valid (TODO).
    /// Returns boot option description and device path of the option.
    pub fn boot_entry_info(entry: &[u8]) -> (String, &DevicePath) {
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
