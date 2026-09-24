use core::time::Duration;

use uefi::Status;

use crate::os::boot_options::BootOptions;
use crate::os::executor::Executor;

pub async fn reboot_to_os() -> ! {
    let next = BootOptions::reboot_target();
    if let Some(next) = next {
        // Reboot to next boot option.
        BootOptions::set_next(next);
    } else {
        log::warn!(
            "Did not find a valid boot order entry! current: {}",
            BootOptions::current()
        );
        log::warn!("{:?}", BootOptions::order());
        Executor::sleep(Duration::from_secs(100)).await;
    }
    reset();
}

pub fn reset() -> ! {
    unsafe {
        // Standard x86 hardware resets:
        // 1. PCI reset register 0xCF9: write 0x02 (SYS_RST) followed by 0x06 (SYS_RST | RST_CPU)
        // NOTE: NEVER write 0x0E here because bit 3 (FULL_RST) asserts SLP_S5#, which causes ATX power off (shutdown)!
        for _ in 0..10 {
            crate::os::arch::io::outb(0xCF9, 0x02);
            for _ in 0..1000 {
                crate::os::arch::io::pause();
            }
            crate::os::arch::io::outb(0xCF9, 0x06);
            for _ in 0..1000 {
                crate::os::arch::io::pause();
            }
        }

        // 2. 8042 PS/2 keyboard controller pulse CPU reset line
        for _ in 0..10 {
            for _ in 0..1000 {
                if (crate::os::arch::io::inb(0x64) & 0x02) == 0 {
                    break;
                }
                crate::os::arch::io::pause();
            }
            crate::os::arch::io::outb(0x64, 0xFE);
            for _ in 0..1000 {
                crate::os::arch::io::pause();
            }
        }
    }
    uefi::runtime::reset(uefi::runtime::ResetType::COLD, Status::SUCCESS, None)
}

pub fn shutdown() -> ! {
    unsafe {
        // Port 0xCF9 with 0x0E asserts SLP_S5# to command ATX power off (soft off)
        crate::os::arch::io::outb(0xCF9, 0x0E);
    }
    uefi::runtime::reset(uefi::runtime::ResetType::SHUTDOWN, Status::SUCCESS, None)
}
