use spin::Mutex;
use uefi::Status;
use uefi::runtime::ResetType;

use crate::arch::io::{cli, hlt, inb, outb, outw};
use crate::boot_options::BootOptions;
use crate::hal::NativeHal;
use pixie_drivers::acpi::AcpiInfo;

static ACPI_INFO: Mutex<Option<AcpiInfo>> = Mutex::new(None);

pub fn init(rsdp_addr: Option<u64>) {
    if let Some(rsdp) = rsdp_addr {
        crate::arch::paging::map_mmio(rsdp, 0x10000);
        if let Some(info) = AcpiInfo::parse_from_rsdp(rsdp, &NativeHal) {
            log::info!(
                "Parsed ACPI FADT/DSDT: PM1a={:?}, PM1b={:?}, SLP_TYPa={:?}, SLP_TYPb={:?}",
                info.pm1a_cnt_blk,
                info.pm1b_cnt_blk,
                info.slp_typa,
                info.slp_typb
            );
            *ACPI_INFO.lock() = Some(info);
        }
    }
}

fn delay_io(loops: usize) {
    for _ in 0..loops {
        unsafe {
            let _ = inb(0x80);
        }
    }
}

pub async fn reboot_to_os() -> ! {
    if let Some(next) = BootOptions::reboot_target() {
        log::info!("Setting BootNext to Boot{:04X}", next);
        BootOptions::set_next(next);
    }
    reset()
}

pub fn reset() -> ! {
    log::info!("Resetting system...");
    unsafe {
        cli();
    }

    if let Some(acpi) = *ACPI_INFO.lock() {
        acpi.reset(&NativeHal);
        delay_io(20_000);
    }

    unsafe {
        // 1. PCI Reset Control Register (0xCF9)
        let cf9 = inb(0xCF9) & !0x06;
        outb(0xCF9, cf9 | 0x02);
        delay_io(10_000);
        outb(0xCF9, cf9 | 0x06);
        delay_io(50_000);
        outb(0xCF9, cf9 | 0x0E);
        delay_io(50_000);

        // 2. 8042 PS/2 keyboard controller CPU reset line pulse
        for _ in 0..10_000 {
            if (inb(0x64) & 0x02) == 0 {
                break;
            }
        }
        outb(0x64, 0xFE);
        delay_io(50_000);
    }

    // 3. Fallback to UEFI Runtime Services inside UEFI context wrapper
    crate::arch::gdt::with_uefi_runtime(|| {
        uefi::runtime::reset(ResetType::COLD, Status::SUCCESS, None)
    });

    loop {
        unsafe {
            hlt();
        }
    }
}

pub fn shutdown() -> ! {
    log::info!("Shutting down system...");
    unsafe {
        cli();
    }

    if let Some(acpi) = *ACPI_INFO.lock() {
        acpi.shutdown(&NativeHal);
        delay_io(50_000);
    }

    unsafe {
        // QEMU / Bochs / VirtualBox ACPI poweroff ports
        outw(0x604, 0x2000);
        outw(0xB004, 0x2000);
        outw(0x4004, 0x3400);
        delay_io(50_000);
    }

    // Fallback to UEFI Runtime Services inside UEFI context wrapper
    crate::arch::gdt::with_uefi_runtime(|| {
        uefi::runtime::reset(ResetType::SHUTDOWN, Status::SUCCESS, None)
    });

    loop {
        unsafe {
            hlt();
        }
    }
}
