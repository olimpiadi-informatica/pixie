use core::sync::atomic::{AtomicU64, Ordering};

pub static IRQ_FLAGS: AtomicU64 = AtomicU64::new(0);

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    type_attr: u8,
    offset_mid: u16,
    offset_high: u32,
    reserved: u32,
}

impl IdtEntry {
    const fn missing() -> Self {
        Self {
            offset_low: 0,
            selector: 0,
            ist: 0,
            type_attr: 0,
            offset_mid: 0,
            offset_high: 0,
            reserved: 0,
        }
    }

    fn set_handler(&mut self, handler: usize) {
        self.offset_low = (handler & 0xFFFF) as u16;
        self.selector = 0x08;
        self.ist = 0;
        self.type_attr = 0x8E;
        self.offset_mid = ((handler >> 16) & 0xFFFF) as u16;
        self.offset_high = (handler >> 32) as u32;
        self.reserved = 0;
    }
}

#[repr(C, align(16))]
struct Idt([IdtEntry; 256]);

static mut IDT: Idt = Idt([IdtEntry::missing(); 256]);

#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

#[repr(C)]
pub struct InterruptFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub vector: u64,
    pub error_code: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

#[cold]
#[inline(never)]
fn handle_exception(frame: &InterruptFrame) -> ! {
    let v = frame.vector;
    let mut cr2: u64 = 0;
    if v == 14 {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            core::arch::asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack, preserves_flags));
        }
    }
    panic!(
        "CPU Exception #{} (err={:#x}) at RIP={:#x}, CS={:#x}, RFLAGS={:#x}, RSP={:#x}, CR2={:#x}",
        v, frame.error_code, frame.rip, frame.cs, frame.rflags, frame.rsp, cr2
    );
}

const ISR_STUB_SIZE: usize = 9;

#[unsafe(no_mangle)]
extern "sysv64" fn rust_interrupt_handler(frame: &mut InterruptFrame) {
    let v = frame.vector & 0xff;
    frame.vector = v;
    if v < 32 {
        handle_exception(frame);
    } else if v == 32 {
        IRQ_FLAGS.fetch_or(1, Ordering::Relaxed);
        crate::watchdog::check();
        super::apic::send_eoi();
    } else if v == 255 {
        // Intel SDM §10.9: Do NOT send EOI on the APIC Spurious Interrupt Vector.
    } else {
        if v < 96 {
            IRQ_FLAGS.fetch_or(1 << (v - 32), Ordering::Relaxed);
        }
        super::apic::send_eoi();
    }
}

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".balign 16",
    "isr_common_stub:",
    "cld",
    "push rax",
    "push rbx",
    "push rcx",
    "push rdx",
    "push rsi",
    "push rdi",
    "push rbp",
    "push r8",
    "push r9",
    "push r10",
    "push r11",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "mov rdi, rsp",
    "mov rbp, rsp",
    "and rsp, -16",
    "call rust_interrupt_handler",
    "mov rsp, rbp",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop r11",
    "pop r10",
    "pop r9",
    "pop r8",
    "pop rbp",
    "pop rdi",
    "pop rsi",
    "pop rdx",
    "pop rcx",
    "pop rbx",
    "pop rax",
    "add rsp, 16",
    "iretq",
    // Each of the 256 stubs is fixed at 9 bytes:
    //   - 2 bytes: `66 90` (2-byte NOP when CPU pushed an error code) or `6a 00` (`push 0`)
    //   - 2 bytes: `6a <vec>` (`push imm8`, masked back to 0..=255 in `rust_interrupt_handler`)
    //   - 5 bytes: `e9 <rel32>` (`jmp isr_common_stub`)
    ".global isr_stubs_start",
    "isr_stubs_start:",
    ".set vec, 0",
    ".rept 256",
    ".if (vec == 8) || (vec == 10) || (vec == 11) || (vec == 12) || (vec == 13) || (vec == 14) || (vec == 17) || (vec == 21) || (vec == 29) || (vec == 30)",
    "    .byte 0x66, 0x90",
    ".else",
    "    .byte 0x6a, 0x00",
    ".endif",
    "    .byte 0x6a, vec",
    "    .byte 0xe9",
    "    .long isr_common_stub - (. + 4)",
    "    .set vec, vec + 1",
    ".endr",
    ".global isr_stubs_end",
    "isr_stubs_end:",
);

#[cfg(target_arch = "x86_64")]
unsafe extern "C" {
    fn isr_stubs_start();
    fn isr_stubs_end();
}

pub unsafe fn init() {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        let base = isr_stubs_start as *const () as usize;
        let end = isr_stubs_end as *const () as usize;
        debug_assert_eq!(end - base, 256 * ISR_STUB_SIZE);

        for vec in 0..256 {
            IDT.0[vec].set_handler(base + vec * ISR_STUB_SIZE);
        }

        let idtr = DescriptorTablePointer {
            limit: (core::mem::size_of::<Idt>() - 1) as u16,
            base: (&raw const IDT.0) as u64,
        };

        core::arch::asm!("lidt [{}]", in(reg) &idtr, options(nostack, preserves_flags));
    }
}
