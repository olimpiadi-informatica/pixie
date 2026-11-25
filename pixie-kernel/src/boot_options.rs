use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use spin::Mutex;

use uefi::runtime::{self, VariableAttributes, VariableVendor};
use uefi::{CString16, cstr16};

struct CachedBootVars {
    current: Option<u16>,
    order: Vec<u16>,
    entries: BTreeMap<u16, Box<[u8]>>,
}

static BOOT_CACHE: Mutex<Option<CachedBootVars>> = Mutex::new(None);

pub struct BootOptions;

impl BootOptions {
    pub fn init_pre_ebs() {
        let current =
            runtime::get_variable_boxed(cstr16!("BootCurrent"), &VariableVendor::GLOBAL_VARIABLE)
                .ok()
                .and_then(|(b, _)| {
                    if b.len() >= 2 {
                        Some(u16::from_le_bytes([b[0], b[1]]))
                    } else {
                        None
                    }
                });

        let order: Vec<u16> =
            runtime::get_variable_boxed(cstr16!("BootOrder"), &VariableVendor::GLOBAL_VARIABLE)
                .ok()
                .map(|(b, _)| {
                    b.as_chunks::<2>()
                        .0
                        .iter()
                        .map(|&x| u16::from_le_bytes(x))
                        .collect()
                })
                .unwrap_or_default();

        let mut entries = BTreeMap::new();
        for &id in &order {
            if let Ok(name) = CString16::try_from(format!("Boot{id:04X}").as_str())
                && let Ok((data, _)) =
                    runtime::get_variable_boxed(&name, &VariableVendor::GLOBAL_VARIABLE)
            {
                entries.insert(id, data);
            }
        }
        for id in 0u16..32 {
            if entries.contains_key(&id) {
                continue;
            }
            if let Ok(name) = CString16::try_from(format!("Boot{id:04X}").as_str())
                && let Ok((data, _)) =
                    runtime::get_variable_boxed(&name, &VariableVendor::GLOBAL_VARIABLE)
            {
                entries.insert(id, data);
            }
        }

        *BOOT_CACHE.lock() = Some(CachedBootVars {
            current,
            order,
            entries,
        });
    }

    pub fn current() -> Option<u16> {
        if let Some(cache) = BOOT_CACHE.lock().as_ref() {
            return cache.current;
        }
        let Ok((boot_current, _)) = crate::arch::gdt::with_uefi_runtime(|| {
            runtime::get_variable_boxed(cstr16!("BootCurrent"), &VariableVendor::GLOBAL_VARIABLE)
        }) else {
            return None;
        };
        if boot_current.len() >= 2 {
            Some(u16::from_le_bytes([boot_current[0], boot_current[1]]))
        } else {
            None
        }
    }

    pub fn order() -> Vec<u16> {
        if let Some(cache) = BOOT_CACHE.lock().as_ref() {
            return cache.order.clone();
        }
        let Ok((boot_order, _)) = crate::arch::gdt::with_uefi_runtime(|| {
            runtime::get_variable_boxed(cstr16!("BootOrder"), &VariableVendor::GLOBAL_VARIABLE)
        }) else {
            return Vec::new();
        };
        boot_order
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&x| u16::from_le_bytes(x))
            .collect()
    }

    pub fn set_order(order: &[u16]) {
        if let Some(cache) = BOOT_CACHE.lock().as_mut() {
            cache.order = order.to_vec();
        }
        let bytes = order
            .iter()
            .flat_map(|x| x.to_le_bytes())
            .collect::<Vec<_>>();
        let _ = crate::arch::gdt::with_uefi_runtime(|| {
            runtime::set_variable(
                cstr16!("BootOrder"),
                &VariableVendor::GLOBAL_VARIABLE,
                VariableAttributes::NON_VOLATILE
                    | VariableAttributes::BOOTSERVICE_ACCESS
                    | VariableAttributes::RUNTIME_ACCESS,
                &bytes,
            )
        });
    }

    pub fn set_next(next: u16) {
        let bytes = next.to_le_bytes();
        let _ = crate::arch::gdt::with_uefi_runtime(|| {
            runtime::set_variable(
                cstr16!("BootNext"),
                &VariableVendor::GLOBAL_VARIABLE,
                VariableAttributes::NON_VOLATILE
                    | VariableAttributes::BOOTSERVICE_ACCESS
                    | VariableAttributes::RUNTIME_ACCESS,
                &bytes,
            )
        });
    }

    pub fn reboot_target() -> Option<u16> {
        let current = Self::current();
        let order = Self::order();
        for id in order {
            if current == Some(id) {
                continue;
            }
            let entry = Self::get(id);
            let Some(desc) = Self::boot_entry_info(&entry) else {
                continue;
            };
            let is_network = desc.contains("PXE")
                || desc.contains("Network")
                || desc.contains("HTTP")
                || desc.contains("IPv4")
                || desc.contains("IPv6");
            if !is_network {
                return Some(id);
            }
        }
        None
    }

    pub fn get(id: u16) -> Box<[u8]> {
        if let Some(cache) = BOOT_CACHE.lock().as_ref()
            && let Some(entry) = cache.entries.get(&id)
        {
            return entry.clone();
        }
        let Ok(name) = CString16::try_from(format!("Boot{id:04X}").as_str()) else {
            return Box::new([]);
        };
        let Ok((val, _)) = crate::arch::gdt::with_uefi_runtime(|| {
            runtime::get_variable_boxed(&name, &VariableVendor::GLOBAL_VARIABLE)
        }) else {
            return Box::new([]);
        };
        val
    }

    pub fn set(id: u16, data: &[u8]) {
        if let Some(cache) = BOOT_CACHE.lock().as_mut() {
            cache.entries.insert(id, data.into());
        }
        if let Ok(name) = CString16::try_from(format!("Boot{id:04X}").as_str()) {
            let _ = crate::arch::gdt::with_uefi_runtime(|| {
                runtime::set_variable(
                    &name,
                    &VariableVendor::GLOBAL_VARIABLE,
                    VariableAttributes::NON_VOLATILE
                        | VariableAttributes::BOOTSERVICE_ACCESS
                        | VariableAttributes::RUNTIME_ACCESS,
                    data,
                )
            });
        }
    }

    pub fn boot_entry_info(entry: &[u8]) -> Option<String> {
        let (code_units, _) = Self::parse_load_option_header(entry)?;
        String::from_utf16(&code_units).ok()
    }

    pub fn current_boot_mac() -> Option<[u8; 6]> {
        let current = Self::current()?;
        let entry = Self::get(current);
        Self::entry_mac(&entry)
    }

    fn entry_mac(entry: &[u8]) -> Option<[u8; 6]> {
        let (_, fp_list) = Self::parse_load_option_header(entry)?;
        let mut pos = 0;
        while pos + 4 <= fp_list.len() {
            let dev_type = fp_list[pos];
            let sub_type = fp_list[pos + 1];
            let node_len = u16::from_le_bytes([fp_list[pos + 2], fp_list[pos + 3]]) as usize;
            if node_len < 4 || pos + node_len > fp_list.len() {
                break;
            }
            if dev_type == 0x7F && sub_type == 0xFF {
                break;
            }
            // Messaging Device Path (0x03), MAC Address (0x0B): payload starts with 32-byte EFI_MAC_ADDRESS
            if dev_type == 0x03 && sub_type == 0x0B && node_len >= 10 {
                let mac = &fp_list[pos + 4..pos + 10];
                if crate::boot_info::is_valid_mac(mac) {
                    let mut out = [0u8; 6];
                    out.copy_from_slice(mac);
                    return Some(out);
                }
            }
            pos += node_len;
        }
        None
    }

    fn parse_load_option_header(entry: &[u8]) -> Option<(Vec<u16>, &[u8])> {
        if entry.len() < 8 {
            return None;
        }
        let fp_len = u16::from_le_bytes([entry[4], entry[5]]) as usize;
        let mut offset = 6;
        let mut code_units = Vec::new();
        let mut nul_found = false;
        while offset + 1 < entry.len() {
            let ch = u16::from_le_bytes([entry[offset], entry[offset + 1]]);
            offset += 2;
            if ch == 0 {
                nul_found = true;
                break;
            }
            code_units.push(ch);
        }
        if !nul_found {
            return None;
        }
        let fp_end = offset.checked_add(fp_len)?.min(entry.len());
        Some((code_units, &entry[offset..fp_end]))
    }
}
