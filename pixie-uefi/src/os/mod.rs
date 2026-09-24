use alloc::vec::Vec;
use core::future::Future;
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;

use uefi::proto::console::gop::GraphicsOutput;

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

    // 1. Locate GOP handles
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

    if let Ok(handle) = uefi::boot::get_handle_for_protocol::<GraphicsOutput>() {
        if !gop_handles.contains(&handle) {
            gop_handles.push(handle);
        }
    }

    for handle in uefi::boot::find_handles::<GraphicsOutput>().unwrap_or_default() {
        if !gop_handles.contains(&handle) {
            gop_handles.push(handle);
        }
    }

    uefi::println!("GOP Handles Found: {}", gop_handles.len());

    if gop_handles.is_empty() {
        uefi::println!("No GOP found! Connecting controllers via DevicePath...");
        let mut connected = 0;
        if let Ok(all_handles) =
            uefi::boot::find_handles::<uefi::proto::device_path::DevicePath>()
        {
            for handle in all_handles {
                if uefi::boot::connect_controller(handle, None, None, true).is_ok() {
                    connected += 1;
                }
            }
        }
        for handle in uefi::boot::find_handles::<GraphicsOutput>().unwrap_or_default() {
            if !gop_handles.contains(&handle) {
                gop_handles.push(handle);
            }
        }
        uefi::println!(
            "  Connected {} device paths. GOP handles now: {}",
            connected,
            gop_handles.len()
        );
    }

    let open_gop = |handle: uefi::Handle| -> Option<uefi::boot::ScopedProtocol<GraphicsOutput>> {
        if let Ok(gop) = uefi::boot::open_protocol_exclusive::<GraphicsOutput>(handle) {
            return Some(gop);
        }
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
        }
    };

    // Print diagnostic info for each GOP handle
    for (idx, &handle) in gop_handles.iter().enumerate() {
        if let Some(mut gop) = open_gop(handle) {
            let cur = gop.current_mode_info();
            let (cw, ch) = cur.resolution();
            let mode_count = gop.modes().count();
            let mut fb = gop.frame_buffer();
            let fb_base = fb.as_mut_ptr() as u64;
            let fb_size = fb.size();
            uefi::println!(
                "GOP[{}]: cur={}x{} stride={} FB=0x{:X} ({}K) modes={}",
                idx,
                cw,
                ch,
                cur.stride(),
                fb_base,
                fb_size / 1024,
                mode_count
            );

            let best_mode = gop
                .modes()
                .filter(|m| {
                    let (w, h) = m.info().resolution();
                    w >= 640 && h >= 480
                })
                .max_by_key(|m| {
                    let (w, h) = m.info().resolution();
                    score_mode(w, h, Some((cw, ch)))
                });

            if let Some(m) = best_mode {
                let (bw, bh) = m.info().resolution();
                uefi::println!(
                    "  -> Best Mode: {}x{} (score: {})",
                    bw,
                    bh,
                    score_mode(bw, bh, Some((cw, ch)))
                );
            } else {
                uefi::println!("  -> No mode >= 640x480 found");
            }
        } else {
            uefi::println!("GOP[{}]: OpenProtocol FAILED", idx);
        }
    }

    // 2. Network SNP handles
    let snp_handles =
        uefi::boot::find_handles::<uefi::proto::network::snp::SimpleNetwork>().unwrap_or_default();
    uefi::print!("SNP Network: {} handle(s)", snp_handles.len());
    let mut uefi_mac = None;
    for &h in &snp_handles {
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
            let m = snp.mode();
            let mac = &m.current_address.0[..6];
            if mac != [0; 6] && (mac[0] & 1) == 0 {
                let mut mac_arr = [0u8; 6];
                mac_arr.copy_from_slice(mac);
                uefi_mac = Some(mac_arr);
                uefi::print!(
                    " | MAC: {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X} (media_present: {:?})",
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
        }
    }
    uefi::println!("");

    // 3. Locate ACPI RSDP pointer
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
    uefi::println!("ACPI RSDP Address: 0x{:X}", rsdp_addr.unwrap_or(0));

    // 4. Calibrate TSC
    Timer::ensure_init();
    let tsc_ticks_per_micro = Timer::ticks_per_micro();
    uefi::println!("TSC Calibrated: {} ticks/us", tsc_ticks_per_micro);

    // 5. Interactive Debug Busy-Wait
    uefi::println!("----------------------------------------------------------------------");
    uefi::println!("DEBUG PAUSE: Press SPACE to PAUSE indefinitely. Any other key to resume.");
    uefi::print!("Auto-continuing in: ");

    let mut remaining = 20; // 20 seconds
    let mut paused = false;

    loop {
        let key = uefi::system::with_stdin(|stdin| stdin.read_key().ok().flatten());
        if let Some(key) = key {
            match key {
                uefi::proto::console::text::Key::Printable(c) if u16::from(c) == b' ' as u16 => {
                    paused = !paused;
                    if paused {
                        uefi::println!("\n*** PAUSED by user. Press any key to resume... ***");
                    } else {
                        uefi::println!("\n*** RESUMING... ***");
                        break;
                    }
                }
                _ => {
                    uefi::println!("\n*** Key pressed: continuing immediately! ***");
                    break;
                }
            }
        }

        if !paused {
            uefi::print!("{}s ", remaining);
            if remaining == 0 {
                uefi::println!("\n*** Timeout: continuing now. ***");
                break;
            }
            remaining -= 1;
        }

        uefi::boot::stall(Duration::from_secs(1));
    }

    // 6. Transition to selected GOP framebuffer
    let mut selected_fb = None;
    let mut _active_gop = None;

    for &handle in &gop_handles {
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
                    let (w, h) = m.info().resolution();
                    w >= 640 && h >= 480
                })
                .max_by_key(|m| {
                    let (w, h) = m.info().resolution();
                    score_mode(w, h, current_res)
                });

            if let Some(mode) = best_mode {
                let (bw, bh) = mode.info().resolution();
                uefi::println!("[1/4] Applying GOP mode {}x{} on handle {:?}...", bw, bh, handle);
                let res = gop.set_mode(&mode);
                uefi::println!("      set_mode result: {:?}", res);
                uefi::boot::stall(Duration::from_secs(1));
            }

            let mode_info = gop.current_mode_info();
            let (fb_width, fb_height) = mode_info.resolution();
            let fb_stride = mode_info.stride() as u32;
            let mut fb = gop.frame_buffer();
            let fb_ptr = fb.as_mut_ptr();
            let fb_size = fb.size();
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

    // Fallback: If GOP is absent, use standard VGA text mode buffer at physical 0xB8000 (80x25)
    let (fb_base, fb_size, fb_width, fb_height, fb_stride) = match selected_fb {
        Some(fb) => fb,
        None => (0xB8000, 80 * 25 * 2, 80, 25, 80),
    };

    // Step 2: Switch ConsoleControl if GOP is active
    if let Some(mut cc) = cc_protocol {
        if selected_fb.is_some() {
            uefi::println!("[2/4] Switching ConsoleControl to Graphics mode (1)...");
            let res = unsafe { (cc.set_mode)(&mut *cc, 1) };
            uefi::println!("      ConsoleControl set_mode result: {:?}", res);
            uefi::boot::stall(Duration::from_secs(1));
        } else {
            uefi::println!("[2/4] GOP is NOT active; leaving ConsoleControl in Text mode!");
            uefi::boot::stall(Duration::from_secs(1));
        }
    }

    // Step 3: Save BootInfo
    uefi::println!("[3/4] Storing BootInfo (fb_base=0x{:X}, size={}K)...", fb_base, fb_size / 1024);
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

    // Step 4: Exit Boot Services
    uefi::println!("[4/4] Calling exit_boot_services (transferring to bare metal)...");
    uefi::println!("      (If system halts here, exit_boot_services or arch::init faulted)");
    uefi::boot::stall(Duration::from_secs(2));

    let memory_map = unsafe { uefi::boot::exit_boot_services(None) };

    // 6. Kernel Architecture Initialization
    unsafe {
        arch::init(); // GDT, IDT (with lightweight ISRs), Local APIC
    }

    // 7. Memory Allocators
    memory::init(&memory_map);

    // 8. Console & UI & Watchdog
    ui::init(boot_info); // GOP Framebuffer UI
    logger::init(); // 16550 UART COM1
    watchdog::init();

    log::info!("Pixie Bare-Metal Kernel initialized in 64-bit Long Mode");
    log::info!(
        "Framebuffer: {}x{} (stride: {}) at 0x{:X}",
        fb_width,
        fb_height,
        fb_stride,
        fb_base as usize
    );

    let mem_stats = memory::stats();
    log::info!(
        "Physical Memory: {} MB usable, {} MB reserved",
        (mem_stats.used + mem_stats.free) / (1024 * 1024),
        mem_stats.other / (1024 * 1024)
    );

    // 9. Start Local APIC periodic timer (5ms = 200 Hz) for smooth UI & watchdog
    unsafe {
        arch::apic::start_periodic_timer(32, 5_000);
        arch::io::sti(); // Enable interrupts
    }

    // 10. Initialize network stack
    net::init();

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
