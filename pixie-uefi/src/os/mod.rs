use alloc::vec::Vec;
use core::fmt::Write;
use core::future::Future;
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;

use uefi::mem::memory_map::MemoryMap;
use uefi::proto::console::gop::{GraphicsOutput, PixelFormat};

use self::boot_info::BootInfo;
use self::error::Result;
use self::executor::Executor;
use self::timer::Timer;

pub mod arch;
pub mod boot_info;
pub mod boot_options;
pub mod disk;
pub mod error;
pub mod executor;
mod font;
pub mod input;
mod logger;
pub mod memory;
pub mod net;
pub mod panic;
pub mod pci;
pub mod raw_fb;
mod send_wrapper;
mod timer;
pub mod ui;
pub mod util;
pub mod watchdog;

use uefi::proto::unsafe_protocol;

#[unsafe_protocol("f42f7782-012e-4c22-b5e8-56e6d1c961b6")]
#[repr(C)]
struct ConsoleControl {
    get_mode: unsafe extern "efiapi" fn(
        *mut ConsoleControl,
        *mut u32,
        *mut bool,
        *mut bool,
    ) -> uefi::Status,
    set_mode: unsafe extern "efiapi" fn(*mut ConsoleControl, u32) -> uefi::Status,
    lock_std_in: unsafe extern "efiapi" fn(*mut ConsoleControl, *const u16) -> uefi::Status,
}

fn score_mode(w: usize, h: usize, current_res: Option<(usize, usize)>) -> i64 {
    if w < 640 || h < 480 {
        return -1_000_000;
    }
    if w < h {
        return -500_000;
    }
    // Prioritize highest resolution (usually the display native resolution), up to 1920x1200
    if w <= 1920 && h <= 1200 {
        let mut score = 1_000_000_i64 + (w * h) as i64;
        if let Some((cw, ch)) = current_res {
            if cw == w && ch == h {
                score += 1_000;
            }
        }
        score
    } else {
        // Too big (> 1920x1200): deprioritize, preferring the smallest oversized mode
        500_000_i64 - ((w * h) / 100) as i64
    }
}

static INITIALIZED: AtomicBool = AtomicBool::new(false);

pub fn start<F, Fut>(mut f: F) -> !
where
    F: FnMut() -> Fut + 'static,
    Fut: Future<Output = Result<()>>,
{
    assert!(!INITIALIZED.swap(true, Ordering::Relaxed));

    // Initialize UEFI helpers while Boot Services are alive
    uefi::helpers::init().unwrap();

    // 0. Initialize UEFI helpers while Boot Services are alive
    let _ = uefi::system::with_stdout(|stdout| stdout.clear());

    uefi::println!("==================== PIXIE UEFI BOOT DIAGNOSTICS ====================");
    uefi::println!(
        "Firmware: {} | Vendor Rev: 0x{:X} | UEFI Rev: {}",
        uefi::system::firmware_vendor(),
        uefi::system::firmware_revision(),
        uefi::system::uefi_revision()
    );

    // Check ConsoleControl protocol without modifying mode yet
    let mut cc_protocol = None;
    if let Ok(handle) = uefi::boot::get_handle_for_protocol::<ConsoleControl>() {
        let params = uefi::boot::OpenProtocolParams {
            handle,
            agent: uefi::boot::image_handle(),
            controller: None,
        };
        if let Ok(mut cc) = unsafe {
            uefi::boot::open_protocol::<ConsoleControl>(
                params,
                uefi::boot::OpenProtocolAttributes::GetProtocol,
            )
        } {
            let mut mode = 999u32;
            let mut uga = false;
            let mut locked = false;
            let status = unsafe { (cc.get_mode)(&mut *cc, &mut mode, &mut uga, &mut locked) };
            uefi::println!(
                "ConsoleControl: PRESENT (status: {:?}, current_mode: {}, uga: {})",
                status,
                match mode {
                    0 => "Text (0)",
                    1 => "Graphics (1)",
                    _ => "Unknown",
                },
                uga
            );
            cc_protocol = Some(cc);
        } else {
            uefi::println!("ConsoleControl: OpenProtocol FAILED");
        }
    } else {
        uefi::println!("ConsoleControl: NOT SUPPORTED by firmware");
    }

    // 1. Locate GOP handles tied to the active console (SimpleTextOutput)
    let mut gop_handles: Vec<uefi::Handle> = Vec::new();

    if let Ok(text_handles) = uefi::boot::find_handles::<uefi::proto::console::text::Output>() {
        for handle in text_handles {
            let params = uefi::boot::OpenProtocolParams {
                handle,
                agent: uefi::boot::image_handle(),
                controller: None,
            };
            if unsafe {
                uefi::boot::open_protocol::<GraphicsOutput>(
                    params,
                    uefi::boot::OpenProtocolAttributes::GetProtocol,
                )
                .is_ok()
            } {
                if !gop_handles.contains(&handle) {
                    gop_handles.push(handle);
                }
            }
        }
    }

    // Fallback: If no console text handle has GOP, check the default GOP protocol handle
    if gop_handles.is_empty() {
        if let Ok(handle) = uefi::boot::get_handle_for_protocol::<GraphicsOutput>() {
            gop_handles.push(handle);
        }
    }

    uefi::println!("[DBG 1] Found {} GOP console handle(s)", gop_handles.len());

    let open_gop = |handle: uefi::Handle| -> Option<core::mem::ManuallyDrop<uefi::boot::ScopedProtocol<GraphicsOutput>>> {
        let params = uefi::boot::OpenProtocolParams {
            handle,
            agent: uefi::boot::image_handle(),
            controller: None,
        };
        unsafe {
            uefi::boot::open_protocol::<GraphicsOutput>(
                params,
                uefi::boot::OpenProtocolAttributes::GetProtocol,
            )
            .ok()
            .map(core::mem::ManuallyDrop::new)
        }
    };

    // Print diagnostic info for each GOP handle
    for (idx, &handle) in gop_handles.iter().enumerate() {
        uefi::println!("[DBG 3.{}] Opening GOP handle {:?}", idx, handle);
        if let Some(mut gop) = open_gop(handle) {
            uefi::println!("[DBG 3.{}] GOP handle opened successfully", idx);
            let cur = gop.current_mode_info();
            let (cw, ch) = cur.resolution();
            let mode_count = gop.modes().count();
            let is_blt_only = cur.pixel_format() == PixelFormat::BltOnly;
            let (fb_base, fb_size) = if is_blt_only {
                (0, 0)
            } else {
                let mut fb = gop.frame_buffer();
                (fb.as_mut_ptr() as u64, fb.size())
            };
            uefi::println!(
                "[DBG 3.{}] cur={}x{} stride={} FB=0x{:X} ({}K, blt_only={}) modes={}",
                idx,
                cw,
                ch,
                cur.stride(),
                fb_base,
                fb_size / 1024,
                is_blt_only,
                mode_count
            );

            let best_mode = gop
                .modes()
                .filter(|m| {
                    let info = m.info();
                    let (w, h) = info.resolution();
                    w >= 640 && h >= 480 && info.pixel_format() != PixelFormat::BltOnly
                })
                .max_by_key(|m| {
                    let (w, h) = m.info().resolution();
                    score_mode(w, h, Some((cw, ch)))
                });

            if let Some(m) = best_mode {
                let (bw, bh) = m.info().resolution();
                uefi::println!(
                    "[DBG 3.{}] Best Mode: {}x{} (score: {})",
                    idx,
                    bw,
                    bh,
                    score_mode(bw, bh, Some((cw, ch)))
                );
            } else {
                uefi::println!("[DBG 3.{}] No direct framebuffer mode >= 640x480 found", idx);
            }
            uefi::println!("[DBG 3.{}] Finished inspecting handle {:?}", idx, handle);
        } else {
            uefi::println!("[DBG 3.{}] OpenProtocol FAILED", idx);
        }
    }
    uefi::println!("[DBG 4] GOP handle inspection complete");

    // 2. Network SNP handles
    uefi::println!("[DBG 5] Discovering SNP network handles...");
    let snp_handles =
        uefi::boot::find_handles::<uefi::proto::network::snp::SimpleNetwork>().unwrap_or_default();
    uefi::println!("[DBG 5] SNP Network: {} handle(s) found", snp_handles.len());
    let mut uefi_mac = None;
    for (i, &h) in snp_handles.iter().enumerate() {
        uefi::println!("[DBG 5.{}] Inspecting SNP handle {:?}", i, h);
        let params = uefi::boot::OpenProtocolParams {
            handle: h,
            agent: uefi::boot::image_handle(),
            controller: None,
        };
        if let Ok(snp) = unsafe {
            uefi::boot::open_protocol::<uefi::proto::network::snp::SimpleNetwork>(
                params,
                uefi::boot::OpenProtocolAttributes::GetProtocol,
            )
        } {
            let snp = core::mem::ManuallyDrop::new(snp);
            let raw_snp =
                &*snp as *const _ as *const uefi_raw::protocol::network::snp::SimpleNetworkProtocol;
            if unsafe { !(*raw_snp).mode.is_null() } {
                let m = snp.mode();
                let mac = &m.current_address.0[..6];
                if mac != [0; 6] && (mac[0] & 1) == 0 {
                    let mut mac_arr = [0u8; 6];
                    mac_arr.copy_from_slice(mac);
                    uefi_mac = Some(mac_arr);
                    uefi::println!(
                        "[DBG 5.{}] Captured MAC: {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X} (media: {:?})",
                        i,
                        mac[0],
                        mac[1],
                        mac[2],
                        mac[3],
                        mac[4],
                        mac[5],
                        m.media_present
                    );
                    break;
                }
            } else {
                uefi::println!("[DBG 5.{}] SNP mode pointer is null, skipping", i);
            }
        } else {
            uefi::println!("[DBG 5.{}] OpenProtocol failed for SNP handle", i);
        }
    }
    uefi::println!("[DBG 6] SNP network discovery complete");

    // 3. Locate ACPI RSDP pointer
    uefi::println!("[DBG 7] Locating ACPI RSDP pointer...");
    let rsdp_addr = uefi::system::with_config_table(|entries| {
        for entry in entries {
            if entry.guid == uefi::table::cfg::ConfigTableEntry::ACPI2_GUID
                || entry.guid == uefi::table::cfg::ConfigTableEntry::ACPI_GUID
            {
                return Some(entry.address as u64);
            }
        }
        None
    });
    uefi::println!("[DBG 7] ACPI RSDP Address: 0x{:X}", rsdp_addr.unwrap_or(0));

    // 4. Calibrate TSC
    uefi::println!("[DBG 8] Calibrating TSC against ACPI/PIT...");
    Timer::ensure_init();
    let tsc_ticks_per_micro = Timer::ticks_per_micro();
    uefi::println!("[DBG 8] TSC Calibrated: {} ticks/us", tsc_ticks_per_micro);

    // 5. Interactive Debug Busy-Wait
    uefi::println!("[DBG 9] Testing stdin availability...");
    let stdin_available = if let Some(st_ptr) = uefi::table::system_table_raw() {
        unsafe { !st_ptr.as_ref().stdin.is_null() }
    } else {
        false
    };
    uefi::println!("[DBG 9] stdin available: {}", stdin_available);

    uefi::println!("----------------------------------------------------------------------");
    if stdin_available {
        uefi::println!("DEBUG PAUSE: Press SPACE to PAUSE indefinitely. Any other key to resume.");
        // Drain any stale keystrokes (e.g. Enter from boot menu)
        while uefi::system::with_stdin(|stdin| stdin.read_key().ok().flatten()).is_some() {}
    } else {
        uefi::println!("DEBUG PAUSE: (No UEFI stdin keyboard detected - auto-countdown)");
    }

    let mut remaining = 10;
    let mut paused = false;

    while remaining > 0 || paused {
        if stdin_available {
            let key = uefi::system::with_stdin(|stdin| stdin.read_key().ok().flatten());
            if let Some(key) = key {
                match key {
                    uefi::proto::console::text::Key::Printable(c) if u16::from(c) == b' ' as u16 => {
                        paused = !paused;
                        if paused {
                            uefi::println!("*** PAUSED by user. Press any key to resume... ***");
                        } else {
                            uefi::println!("*** RESUMING... ***");
                            break;
                        }
                    }
                    _ => {
                        uefi::println!("*** Key pressed: continuing immediately! ***");
                        break;
                    }
                }
            }
        }

        if !paused {
            uefi::println!("[PAUSE] Continuing in {}s...", remaining);
            remaining -= 1;
        }

        uefi::boot::stall(Duration::from_secs(1));
    }
    uefi::println!("[DBG 10] Debug pause finished, transitioning to GOP mode...");

    // 6. Transition to selected GOP framebuffer
    let mut selected_fb = None;
    let mut _active_gop = None;

    uefi::println!("[DBG 11] Entering final GOP mode switch loop...");
    for (idx, &handle) in gop_handles.iter().enumerate() {
        uefi::println!("[DBG 11.{}] Re-opening handle {:?} for final mode set...", idx, handle);
        if let Some(mut gop) = open_gop(handle) {
            let current_res = {
                let info = gop.current_mode_info();
                let (w, h) = info.resolution();
                if w > 0 && h > 0 {
                    Some((w, h))
                } else {
                    None
                }
            };

            let best_mode = gop
                .modes()
                .filter(|m| {
                    let info = m.info();
                    let (w, h) = info.resolution();
                    w >= 640 && h >= 480 && info.pixel_format() != PixelFormat::BltOnly
                })
                .max_by_key(|m| {
                    let (w, h) = m.info().resolution();
                    score_mode(w, h, current_res)
                });

            if let Some(mode) = best_mode {
                let (bw, bh) = mode.info().resolution();
                uefi::println!("[DBG 11.{}] Applying GOP mode {}x{} on handle {:?}...", idx, bw, bh, handle);
                let res = gop.set_mode(&mode);
                uefi::println!("[DBG 11.{}] set_mode result: {:?}", idx, res);
                uefi::boot::stall(Duration::from_secs(1));
            } else {
                uefi::println!("[DBG 11.{}] No acceptable mode found", idx);
            }

            let mode_info = gop.current_mode_info();
            if mode_info.pixel_format() == PixelFormat::BltOnly {
                uefi::println!("[DBG 11.{}] Mode is BltOnly, skipping", idx);
                continue;
            }
            let (fb_width, fb_height) = mode_info.resolution();
            let fb_stride = mode_info.stride() as u32;
            let mut fb = gop.frame_buffer();
            let fb_ptr = fb.as_mut_ptr();
            let fb_size = fb.size();
            uefi::println!(
                "[DBG 11.{}] Framebuffer active: {}x{} stride={} base=0x{:X} size={}K",
                idx, fb_width, fb_height, fb_stride, fb_ptr as usize, fb_size / 1024
            );
            if fb_width > 0 && fb_height > 0 && !fb_ptr.is_null() && fb_size > 0 {
                selected_fb = Some((
                    fb_ptr as u64,
                    fb_size,
                    fb_width as u32,
                    fb_height as u32,
                    fb_stride,
                ));
                _active_gop = Some(gop);
                break;
            }
        }
    }
    uefi::println!("[DBG 12] Final GOP mode selection complete. Has FB: {}", selected_fb.is_some());

    // Step 2: Switch ConsoleControl if GOP is active
    let (fb_base, fb_size, fb_width, fb_height, fb_stride) = match selected_fb {
        Some(fb) => fb,
        None => (0xB8000, 80 * 25 * 2, 80, 25, 80),
    };

    if let Some(mut cc) = cc_protocol {
        if selected_fb.is_some() {
            uefi::println!("[DBG 13] Switching ConsoleControl to Graphics mode (1)...");
            let res = unsafe { (cc.set_mode)(&mut *cc, 1) };
            uefi::println!("[DBG 13] ConsoleControl set_mode result: {:?}", res);
            uefi::boot::stall(Duration::from_secs(1));
        } else {
            uefi::println!("[DBG 13] GOP is NOT active; leaving ConsoleControl in Text mode!");
            uefi::boot::stall(Duration::from_secs(1));
        }
    } else {
        uefi::println!("[DBG 13] ConsoleControl not present");
    }

    // Step 3: Save BootInfo
    uefi::println!("[DBG 14] Storing BootInfo (fb_base=0x{:X}, size={}K)...", fb_base, fb_size / 1024);
    let boot_info = BootInfo {
        framebuffer_base: fb_base,
        framebuffer_size: fb_size,
        fb_width,
        fb_height,
        fb_stride,
        rsdp_addr,
        tsc_ticks_per_micro,
        uefi_mac,
    };
    boot_info::set_boot_info(boot_info);
    uefi::println!("[DBG 14] BootInfo stored successfully");

    // Step 4: Exit Boot Services
    uefi::println!("----------------------------------------------------------------------");
    uefi::println!("[DBG 15] Ready to exit boot services.");
    uefi::println!(
        "         Resolution: {}x{}, Stride: {}, FB: 0x{:X} ({}K)",
        fb_width,
        fb_height,
        fb_stride,
        fb_base,
        fb_size / 1024
    );
    uefi::println!("         Pausing 5 seconds so you can read this screen...");
    for s in (1..=5).rev() {
        uefi::println!("         Exiting boot services in {}s...", s);
        uefi::boot::stall(Duration::from_secs(1));
    }

    let memory_map = unsafe { uefi::boot::exit_boot_services(None) };

    // 6. Direct Framebuffer Early Diagnostics
    raw_fb::init(&boot_info);
    raw_fb::clear_screen(raw_fb::COLOR_DARK_BLUE);
    raw_fb::print("================== PIXIE POST-EBS KERNEL DIAGNOSTICS ==================", raw_fb::COLOR_YELLOW, raw_fb::COLOR_DARK_BLUE);

    let mut w = raw_fb::StackWriter::<128>::new();
    let _ = core::write!(w, "[POST-EBS 1] ExitBootServices OK! Memory map: {} entries", memory_map.len());
    raw_fb::print(w.as_str(), raw_fb::COLOR_GREEN, raw_fb::COLOR_DARK_BLUE);

    raw_fb::print("[POST-EBS 2] Initializing GDT...", raw_fb::COLOR_CYAN, raw_fb::COLOR_DARK_BLUE);
    unsafe { arch::gdt::init(); }
    raw_fb::print("[POST-EBS 2] GDT initialized OK", raw_fb::COLOR_GREEN, raw_fb::COLOR_DARK_BLUE);

    raw_fb::print("[POST-EBS 3] Initializing IDT...", raw_fb::COLOR_CYAN, raw_fb::COLOR_DARK_BLUE);
    unsafe { arch::idt::init(); }
    raw_fb::print("[POST-EBS 3] IDT initialized OK", raw_fb::COLOR_GREEN, raw_fb::COLOR_DARK_BLUE);

    raw_fb::print("[POST-EBS 4] Initializing Local APIC...", raw_fb::COLOR_CYAN, raw_fb::COLOR_DARK_BLUE);
    unsafe { arch::apic::init(); }
    raw_fb::print("[POST-EBS 4] Local APIC initialized OK", raw_fb::COLOR_GREEN, raw_fb::COLOR_DARK_BLUE);

    raw_fb::print("[POST-EBS 5] Initializing Physical Memory & Allocator...", raw_fb::COLOR_CYAN, raw_fb::COLOR_DARK_BLUE);
    memory::init(&memory_map);
    let mem_stats = memory::stats();
    w.clear();
    let _ = core::write!(w, "[POST-EBS 5] Memory ready: Usable: {} MB, Reserved: {} MB", (mem_stats.used + mem_stats.free) / (1024 * 1024), mem_stats.other / (1024 * 1024));
    raw_fb::print(w.as_str(), raw_fb::COLOR_GREEN, raw_fb::COLOR_DARK_BLUE);

    raw_fb::print("[POST-EBS 6] Scanning PCI bus...", raw_fb::COLOR_CYAN, raw_fb::COLOR_DARK_BLUE);
    let pci_devices = pci::scan_pci();
    w.clear();
    let _ = core::write!(w, "[POST-EBS 6] Found {} PCI device(s)", pci_devices.len());
    raw_fb::print(w.as_str(), raw_fb::COLOR_GREEN, raw_fb::COLOR_DARK_BLUE);
    for dev in &pci_devices {
        if dev.class_code == 0x02 { // Network controller
            w.clear();
            let _ = core::write!(w, "  -> NET: {:02x}:{:02x}.{:x} ID: {:04x}:{:04x} class: {:02x}:{:02x}", dev.bus, dev.dev, dev.func, dev.vendor_id, dev.device_id, dev.class_code, dev.subclass);
            raw_fb::print(w.as_str(), raw_fb::COLOR_YELLOW, raw_fb::COLOR_DARK_BLUE);
        }
    }

    raw_fb::print("[POST-EBS 7] Initializing Serial COM1 & Watchdog...", raw_fb::COLOR_CYAN, raw_fb::COLOR_DARK_BLUE);
    logger::init();
    watchdog::init();
    raw_fb::print("[POST-EBS 7] Serial & Watchdog OK", raw_fb::COLOR_GREEN, raw_fb::COLOR_DARK_BLUE);

    raw_fb::print("[POST-EBS 8] Starting APIC Timer & STI...", raw_fb::COLOR_CYAN, raw_fb::COLOR_DARK_BLUE);
    unsafe {
        arch::apic::start_periodic_timer(32, 1_000);
        arch::io::sti();
    }
    raw_fb::print("[POST-EBS 8] Timer running & interrupts enabled OK", raw_fb::COLOR_GREEN, raw_fb::COLOR_DARK_BLUE);

    raw_fb::print("[POST-EBS 9] Initializing Network stack (net::init)...", raw_fb::COLOR_CYAN, raw_fb::COLOR_DARK_BLUE);
    net::init();
    raw_fb::print("[POST-EBS 9] Network stack initialized OK", raw_fb::COLOR_GREEN, raw_fb::COLOR_DARK_BLUE);

    raw_fb::print("[POST-EBS 10] Initializing UI subsystem...", raw_fb::COLOR_CYAN, raw_fb::COLOR_DARK_BLUE);
    // Stall 3 seconds so the user can read all diagnostic lines on the screen!
    let stall_start = timer::Timer::micros();
    while timer::Timer::micros() - stall_start < 3_000_000 {
        arch::io::pause();
    }
    ui::init(boot_info);

    log::info!("Pixie Bare-Metal Kernel initialized in 64-bit Long Mode");
    log::info!(
        "Framebuffer: {}x{} (stride: {}) at 0x{:X}",
        fb_width,
        fb_height,
        fb_stride,
        fb_base as usize
    );

    log::info!(
        "Physical Memory: {} MB usable, {} MB reserved",
        (mem_stats.used + mem_stats.free) / (1024 * 1024),
        mem_stats.other / (1024 * 1024)
    );

    // 11. Spawn core tasks
    Executor::spawn("init", async move {
        loop {
            if let Err(err) = f().await {
                log::error!("Error: {err:?}");
            }
        }
    });

    Executor::spawn("[watchdog]", async move {
        loop {
            watchdog::pet();
            Executor::sleep(Duration::from_secs(5)).await;
        }
    });

    Executor::run()
}
