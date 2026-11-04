use core::sync::atomic::{AtomicBool, Ordering};

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

#[repr(C, align(16))]
struct Gdt([u64; 8]);

static GDT: Gdt = Gdt([
    0x0000_0000_0000_0000, // 0x00: Null descriptor
    0x00AF_9A00_0000_FFFF, // 0x08: 64-bit Kernel Code (DPL 0, L=1, P=1, S=1, E=1, RW=1)
    0x00CF_9200_0000_FFFF, // 0x10: 64-bit Kernel Data (DPL 0, P=1, S=1, RW=1)
    0x00CF_9A00_0000_FFFF, // 0x18: 32-bit Code
    0x00CF_9200_0000_FFFF, // 0x20: 32-bit Data
    0x00AF_9A00_0000_FFFF, // 0x28: 64-bit Code
    0x00CF_9200_0000_FFFF, // 0x30: UEFI-compatible 64-bit Data
    0x00AF_9A00_0000_FFFF, // 0x38: UEFI-compatible 64-bit Code
]);

static mut UEFI_GDTR: DescriptorTablePointer = DescriptorTablePointer { limit: 0, base: 0 };
static mut UEFI_IDTR: DescriptorTablePointer = DescriptorTablePointer { limit: 0, base: 0 };
static mut UEFI_CR3: u64 = 0;
static UEFI_SAVED: AtomicBool = AtomicBool::new(false);

/// Saves UEFI's original GDTR/IDTR/CR3 and loads the kernel GDT containing both `0x08`/`0x10`
/// and UEFI-compatible `0x38`/`0x30` 64-bit Ring-0 Code/Data selectors.
pub unsafe fn init() {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        if !UEFI_SAVED.load(Ordering::SeqCst) {
            let mut gdtr = DescriptorTablePointer { limit: 0, base: 0 };
            let mut idtr = DescriptorTablePointer { limit: 0, base: 0 };
            let cr3: u64;
            core::arch::asm!(
                "sgdt [{}]",
                "sidt [{}]",
                "mov {}, cr3",
                in(reg) &mut gdtr,
                in(reg) &mut idtr,
                out(reg) cr3,
                options(nostack, preserves_flags)
            );
            UEFI_GDTR = gdtr;
            UEFI_IDTR = idtr;
            UEFI_CR3 = cr3;
            UEFI_SAVED.store(true, Ordering::SeqCst);
        }

        load_kernel_gdt();
    }
}

#[cfg(target_arch = "x86_64")]
unsafe fn load_kernel_gdt() {
    let gdtr = DescriptorTablePointer {
        limit: (core::mem::size_of::<Gdt>() - 1) as u16,
        base: GDT.0.as_ptr() as u64,
    };

    unsafe {
        core::arch::asm!(
            "lgdt [{gdtr}]",
            "push 0x08",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            "mov ax, 0x10",
            "mov ds, ax",
            "mov es, ax",
            "mov ss, ax",
            "xor eax, eax",
            "mov fs, ax",
            "mov gs, ax",
            gdtr = in(reg) &gdtr,
            tmp = lateout(reg) _,
            out("rax") _,
        );
    }
}

/// Executes `f` with CPU interrupts disabled, APIC timer masked, and UEFI firmware's original
/// GDTR/IDTR/CR3 restored so UEFI Runtime Services (`SetVariable`, `ResetSystem`) execute safely.
pub fn with_uefi_runtime<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    #[cfg(target_arch = "x86_64")]
    unsafe {
        let rflags: u64;
        core::arch::asm!(
            "pushfq",
            "pop {}",
            "cli",
            out(reg) rflags,
            options(nomem)
        );
        super::apic::set_timer_masked(true);

        let saved = UEFI_SAVED.load(Ordering::SeqCst);
        let mut kern_gdtr = DescriptorTablePointer { limit: 0, base: 0 };
        let mut kern_idtr = DescriptorTablePointer { limit: 0, base: 0 };
        let mut kern_cr3: u64 = 0;

        if saved {
            core::arch::asm!(
                "sgdt [{}]",
                "sidt [{}]",
                "mov {}, cr3",
                in(reg) &mut kern_gdtr,
                in(reg) &mut kern_idtr,
                out(reg) kern_cr3,
                options(nostack, preserves_flags)
            );
            let uefi_gdtr = UEFI_GDTR;
            let uefi_idtr = UEFI_IDTR;
            let uefi_cr3 = UEFI_CR3;
            if uefi_cr3 != 0 && uefi_cr3 != kern_cr3 {
                core::arch::asm!("mov cr3, {}", in(reg) uefi_cr3, options(nostack, preserves_flags));
            }
            if uefi_gdtr.base != 0 {
                core::arch::asm!("lgdt [{}]", in(reg) &uefi_gdtr, options(nostack, preserves_flags));
            }
            if uefi_idtr.base != 0 {
                core::arch::asm!("lidt [{}]", in(reg) &uefi_idtr, options(nostack, preserves_flags));
            }
        }

        let res = f();

        if saved {
            if kern_cr3 != 0 {
                core::arch::asm!("mov cr3, {}", in(reg) kern_cr3, options(nostack, preserves_flags));
            }
            load_kernel_gdt();
            if kern_idtr.base != 0 {
                core::arch::asm!("lidt [{}]", in(reg) &kern_idtr, options(nostack, preserves_flags));
            }
        }

        super::apic::set_timer_masked(false);
        if (rflags & 0x200) != 0 {
            super::io::sti();
        }
        res
    }

    #[cfg(not(target_arch = "x86_64"))]
    {
        f()
    }
}
