use spin::Mutex;
use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams, SearchType};
use uefi::mem::memory_map::{MemoryMapOwned, MemoryType};
use uefi::proto::console::gop::{GraphicsOutput, PixelFormat};
use uefi::proto::device_path::{DevicePath, DeviceSubType, DeviceType};
use uefi::proto::loaded_image::LoadedImage;
use uefi::proto::network::snp::{NetworkState, SimpleNetwork};
use uefi::table::cfg::ConfigTableEntry;
use uefi::{Handle, Identify};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FbPixelFormat {
    Bgr,
    Rgb,
}

#[derive(Clone, Copy, Debug)]
pub struct BootInfo {
    pub framebuffer_base: u64,
    pub framebuffer_size: usize,
    pub fb_width: u32,
    pub fb_height: u32,
    pub fb_stride: u32,
    pub pixel_format: FbPixelFormat,
    pub rsdp_addr: Option<u64>,
    pub uefi_mac: Option<[u8; 6]>,
}

pub static BOOT_INFO: Mutex<Option<BootInfo>> = Mutex::new(None);

pub(crate) fn is_valid_mac(mac: &[u8]) -> bool {
    mac.len() == 6 && mac.iter().any(|&b| b != 0) && mac != [0xFF; 6]
}

fn snp_handle_mac(handle: Handle) -> Option<[u8; 6]> {
    let snp = unsafe {
        boot::open_protocol::<SimpleNetwork>(
            OpenProtocolParams {
                handle,
                agent: boot::image_handle(),
                controller: None,
            },
            OpenProtocolAttributes::GetProtocol,
        )
    }
    .ok()?;
    let mode = snp.mode();
    let cur = &mode.current_address.0[..6];
    let perm = &mode.permanent_address.0[..6];
    let slice = if is_valid_mac(cur) {
        cur
    } else if is_valid_mac(perm) {
        perm
    } else {
        return None;
    };
    let mut mac = [0u8; 6];
    mac.copy_from_slice(slice);
    Some(mac)
}

fn loaded_image_boot_mac() -> Option<[u8; 6]> {
    let li = unsafe {
        boot::open_protocol::<LoadedImage>(
            OpenProtocolParams {
                handle: boot::image_handle(),
                agent: boot::image_handle(),
                controller: None,
            },
            OpenProtocolAttributes::GetProtocol,
        )
    }
    .ok()?;
    let dev_handle = li.device()?;
    if let Ok(dp) = unsafe {
        boot::open_protocol::<DevicePath>(
            OpenProtocolParams {
                handle: dev_handle,
                agent: boot::image_handle(),
                controller: None,
            },
            OpenProtocolAttributes::GetProtocol,
        )
    } {
        for node in dp.node_iter() {
            if node.full_type() == (DeviceType::MESSAGING, DeviceSubType::MESSAGING_MAC_ADDRESS)
                && node.data().len() >= 6
                && is_valid_mac(&node.data()[..6])
            {
                let mut mac = [0u8; 6];
                mac.copy_from_slice(&node.data()[..6]);
                return Some(mac);
            }
        }
    }
    snp_handle_mac(dev_handle)
}

pub fn collect_and_exit_boot_services() -> (BootInfo, MemoryMapOwned) {
    // 1. Disable UEFI watchdog timer and calibrate TSC timer
    let _ = boot::set_watchdog_timer(0, 0x10000, None);
    crate::timer::Timer::ensure_init();

    // 2. Cache UEFI BootCurrent / BootOrder / BootXXXX before exiting boot services
    crate::boot_options::BootOptions::init_pre_ebs();

    // Identify the NIC handle / MAC that actually loaded pixie-client.efi so dual-NIC
    // systems with multiple active SNP handles don't arbitrarily pick handle 0.
    let boot_mac =
        loaded_image_boot_mac().or_else(crate::boot_options::BootOptions::current_boot_mac);

    // 3. Stop UEFI SimpleNetwork (SNP / UNDI) on all handles and capture best active MAC
    let mut best_mac: Option<(u8, [u8; 6])> = None;
    if let Ok(handles) = boot::locate_handle_buffer(SearchType::ByProtocol(&SimpleNetwork::GUID)) {
        for &handle in handles.iter() {
            if let Ok(snp) = unsafe {
                boot::open_protocol::<SimpleNetwork>(
                    OpenProtocolParams {
                        handle,
                        agent: boot::image_handle(),
                        controller: None,
                    },
                    OpenProtocolAttributes::GetProtocol,
                )
            } {
                let mode = snp.mode();
                let cur_mac = &mode.current_address.0[..6];
                let perm_mac = &mode.permanent_address.0[..6];
                let chosen = if is_valid_mac(cur_mac) {
                    Some((cur_mac, true))
                } else if is_valid_mac(perm_mac) {
                    Some((perm_mac, false))
                } else {
                    None
                };

                if let Some((mac_slice, has_current)) = chosen {
                    let mut score = 0u8;
                    if boot_mac.is_some_and(|bm| bm == mac_slice) {
                        score += 16;
                    }
                    if mode.state == NetworkState::INITIALIZED {
                        score += 4;
                    }
                    if !bool::from(mode.media_present_supported) || bool::from(mode.media_present) {
                        score += 2;
                    }
                    if has_current {
                        score += 1;
                    }
                    if best_mac.is_none_or(|(best_score, _)| score > best_score) {
                        let mut m = [0u8; 6];
                        m.copy_from_slice(mac_slice);
                        best_mac = Some((score, m));
                    }
                }

                let _ = snp.receive_filters(
                    uefi::proto::network::snp::ReceiveFlags::empty(),
                    uefi::proto::network::snp::ReceiveFlags::empty(),
                    false,
                    None,
                );
                let _ = snp.shutdown();
                let _ = snp.stop();
            }
        }
    }
    let uefi_mac = best_mac.map(|(_, m)| m).or(boot_mac);

    // 4. Locate GOP Framebuffer
    let (framebuffer_base, framebuffer_size, fb_width, fb_height, fb_stride, pixel_format) =
        if let Ok(handle) = boot::get_handle_for_protocol::<GraphicsOutput>()
            && let Ok(mut gop) = unsafe {
                boot::open_protocol::<GraphicsOutput>(
                    OpenProtocolParams {
                        handle,
                        agent: boot::image_handle(),
                        controller: None,
                    },
                    OpenProtocolAttributes::GetProtocol,
                )
            }
        {
            let best_mode = gop
                .modes()
                .filter(|m| matches!(m.info().pixel_format(), PixelFormat::Bgr | PixelFormat::Rgb))
                .max_by_key(|m| {
                    let (w, h) = m.info().resolution();
                    let pref = if w <= 1920 && h <= 1080 {
                        1_000_000_000
                    } else {
                        0
                    };
                    pref + (w * h)
                });
            if let Some(mode) = best_mode {
                let _ = gop.set_mode(&mode);
            }
            let info = gop.current_mode_info();
            let (w, h) = info.resolution();
            let stride = info.stride();
            let fmt = match info.pixel_format() {
                PixelFormat::Rgb => FbPixelFormat::Rgb,
                _ => FbPixelFormat::Bgr,
            };
            let mut fb = gop.frame_buffer();
            (
                fb.as_mut_ptr() as u64,
                fb.size(),
                w as u32,
                h as u32,
                stride as u32,
                fmt,
            )
        } else {
            (0, 0, 800, 600, 800, FbPixelFormat::Bgr)
        };

    // 5. Find ACPI RSDP physical address from UEFI Configuration Table
    let rsdp_addr = uefi::system::with_config_table(|entries| {
        entries
            .iter()
            .find(|e| e.guid == ConfigTableEntry::ACPI2_GUID)
            .or_else(|| {
                entries
                    .iter()
                    .find(|e| e.guid == ConfigTableEntry::ACPI_GUID)
            })
            .map(|e| e.address as u64)
    });

    // 6. Exit UEFI Boot Services
    let memory_map = unsafe { boot::exit_boot_services(Some(MemoryType::LOADER_DATA)) };

    let info = BootInfo {
        framebuffer_base,
        framebuffer_size,
        fb_width,
        fb_height,
        fb_stride,
        pixel_format,
        rsdp_addr,
        uefi_mac,
    };
    *BOOT_INFO.lock() = Some(info);
    (info, memory_map)
}
