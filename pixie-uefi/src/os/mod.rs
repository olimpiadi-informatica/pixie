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

static INITIALIZED: AtomicBool = AtomicBool::new(false);

pub fn start<F, Fut>(mut f: F) -> !
where
    F: FnMut() -> Fut + 'static,
    Fut: Future<Output = Result<()>>,
{
    assert!(!INITIALIZED.swap(true, Ordering::Relaxed));

    // Initialize UEFI helpers while Boot Services are alive
    uefi::helpers::init().unwrap();

    // 1. Locate and configure GOP framebuffer
    let gop_handles = uefi::boot::find_handles::<GraphicsOutput>().unwrap_or_default();
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

    let mut selected_fb = None;
    for &handle in &gop_handles {
        if let Some(mut gop) = open_gop(handle) {
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
                break;
            }
        }
    }

    if selected_fb.is_none() {
        for &handle in &gop_handles {
            if let Some(mut gop) = open_gop(handle) {
                let valid_mode = gop.modes().find(|m| {
                    let info = m.info();
                    let (w, h) = info.resolution();
                    w > 0 && h > 0
                });
                if let Some(mode) = valid_mode {
                    let _ = gop.set_mode(&mode);
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
                        break;
                    }
                }
            }
        }
    }

    let (fb_base, fb_size, fb_width, fb_height, fb_stride) = selected_fb
        .expect("GraphicsOutput protocol not found or no valid framebuffer mode");

    // 2. Locate ACPI RSDP pointer
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

    // 3. Calibrate TSC before ExitBootServices
    Timer::ensure_init();
    let tsc_ticks_per_micro = Timer::ticks_per_micro();

    // 4. Save BootInfo
    let boot_info = BootInfo {
        framebuffer_base: fb_base,
        framebuffer_size: fb_size,
        fb_width,
        fb_height,
        fb_stride,
        rsdp_addr,
        tsc_ticks_per_micro,
    };
    boot_info::set_boot_info(boot_info);

    // 5. Exit UEFI Boot Services -> Transition to Bare-Metal Kernel!
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

    // 9. Start Local APIC periodic timer for watchdog (e.g. vector 32)
    unsafe {
        arch::apic::start_timer(32, 100_000_000);
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
