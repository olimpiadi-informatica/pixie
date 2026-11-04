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

#[unsafe(no_mangle)]
extern "sysv64" fn rust_interrupt_handler(frame: &mut InterruptFrame) {
    let v = frame.vector;
    if v < 32 {
        let mut cr2: u64 = 0;
        if v == 14 {
            #[cfg(target_arch = "x86_64")]
            unsafe {
                core::arch::asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack, preserves_flags));
            }
        }
        panic!(
            "CPU Exception #{} (err={:#x}) at RIP={:#x}, CS={:#x}, RFLAGS={:#x}, RSP={:#x}, CR2={:#x}",
            v,
            { frame.error_code },
            { frame.rip },
            { frame.cs },
            { frame.rflags },
            { frame.rsp },
            cr2
        );
    } else if v == 32 {
        IRQ_FLAGS.fetch_or(1, Ordering::Relaxed);
        super::apic::send_eoi();
    } else {
        if v < 96 {
            IRQ_FLAGS.fetch_or(1 << (v - 32), Ordering::Relaxed);
        }
        super::apic::send_eoi();
    }
}

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".align 16",
    "isr_common_stub:",
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
);

macro_rules! define_isr {
    ($vec:expr, no_err) => {
        #[cfg(target_arch = "x86_64")]
        core::arch::global_asm!(concat!(
            ".global isr_stub_",
            stringify!($vec),
            "\n",
            "isr_stub_",
            stringify!($vec),
            ":\n",
            "push 0\n",
            "push ",
            stringify!($vec),
            "\n",
            "jmp isr_common_stub\n"
        ));
    };
    ($vec:expr, err) => {
        #[cfg(target_arch = "x86_64")]
        core::arch::global_asm!(concat!(
            ".global isr_stub_",
            stringify!($vec),
            "\n",
            "isr_stub_",
            stringify!($vec),
            ":\n",
            "push ",
            stringify!($vec),
            "\n",
            "jmp isr_common_stub\n"
        ));
    };
}

define_isr!(0, no_err);
define_isr!(1, no_err);
define_isr!(2, no_err);
define_isr!(3, no_err);
define_isr!(4, no_err);
define_isr!(5, no_err);
define_isr!(6, no_err);
define_isr!(7, no_err);
define_isr!(8, err);
define_isr!(9, no_err);
define_isr!(10, err);
define_isr!(11, err);
define_isr!(12, err);
define_isr!(13, err);
define_isr!(14, err);
define_isr!(15, no_err);
define_isr!(16, no_err);
define_isr!(17, err);
define_isr!(18, no_err);
define_isr!(19, no_err);
define_isr!(20, no_err);
define_isr!(21, err);
define_isr!(22, no_err);
define_isr!(23, no_err);
define_isr!(24, no_err);
define_isr!(25, no_err);
define_isr!(26, no_err);
define_isr!(27, no_err);
define_isr!(28, no_err);
define_isr!(29, err);
define_isr!(30, err);
define_isr!(31, no_err);

define_isr!(32, no_err);
define_isr!(33, no_err);
define_isr!(34, no_err);
define_isr!(35, no_err);
define_isr!(36, no_err);
define_isr!(37, no_err);
define_isr!(38, no_err);
define_isr!(39, no_err);
define_isr!(40, no_err);
define_isr!(41, no_err);
define_isr!(42, no_err);
define_isr!(43, no_err);
define_isr!(44, no_err);
define_isr!(45, no_err);
define_isr!(46, no_err);
define_isr!(47, no_err);
define_isr!(48, no_err);
define_isr!(255, no_err);

#[cfg(target_arch = "x86_64")]
unsafe extern "C" {
    fn isr_stub_0();
    fn isr_stub_1();
    fn isr_stub_2();
    fn isr_stub_3();
    fn isr_stub_4();
    fn isr_stub_5();
    fn isr_stub_6();
    fn isr_stub_7();
    fn isr_stub_8();
    fn isr_stub_9();
    fn isr_stub_10();
    fn isr_stub_11();
    fn isr_stub_12();
    fn isr_stub_13();
    fn isr_stub_14();
    fn isr_stub_15();
    fn isr_stub_16();
    fn isr_stub_17();
    fn isr_stub_18();
    fn isr_stub_19();
    fn isr_stub_20();
    fn isr_stub_21();
    fn isr_stub_22();
    fn isr_stub_23();
    fn isr_stub_24();
    fn isr_stub_25();
    fn isr_stub_26();
    fn isr_stub_27();
    fn isr_stub_28();
    fn isr_stub_29();
    fn isr_stub_30();
    fn isr_stub_31();
    fn isr_stub_32();
    fn isr_stub_33();
    fn isr_stub_34();
    fn isr_stub_35();
    fn isr_stub_36();
    fn isr_stub_37();
    fn isr_stub_38();
    fn isr_stub_39();
    fn isr_stub_40();
    fn isr_stub_41();
    fn isr_stub_42();
    fn isr_stub_43();
    fn isr_stub_44();
    fn isr_stub_45();
    fn isr_stub_46();
    fn isr_stub_47();
    fn isr_stub_48();
    fn isr_stub_255();
}

pub unsafe fn init() {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        let stubs: [(usize, unsafe extern "C" fn()); 50] = [
            (0, isr_stub_0),
            (1, isr_stub_1),
            (2, isr_stub_2),
            (3, isr_stub_3),
            (4, isr_stub_4),
            (5, isr_stub_5),
            (6, isr_stub_6),
            (7, isr_stub_7),
            (8, isr_stub_8),
            (9, isr_stub_9),
            (10, isr_stub_10),
            (11, isr_stub_11),
            (12, isr_stub_12),
            (13, isr_stub_13),
            (14, isr_stub_14),
            (15, isr_stub_15),
            (16, isr_stub_16),
            (17, isr_stub_17),
            (18, isr_stub_18),
            (19, isr_stub_19),
            (20, isr_stub_20),
            (21, isr_stub_21),
            (22, isr_stub_22),
            (23, isr_stub_23),
            (24, isr_stub_24),
            (25, isr_stub_25),
            (26, isr_stub_26),
            (27, isr_stub_27),
            (28, isr_stub_28),
            (29, isr_stub_29),
            (30, isr_stub_30),
            (31, isr_stub_31),
            (32, isr_stub_32),
            (33, isr_stub_33),
            (34, isr_stub_34),
            (35, isr_stub_35),
            (36, isr_stub_36),
            (37, isr_stub_37),
            (38, isr_stub_38),
            (39, isr_stub_39),
            (40, isr_stub_40),
            (41, isr_stub_41),
            (42, isr_stub_42),
            (43, isr_stub_43),
            (44, isr_stub_44),
            (45, isr_stub_45),
            (46, isr_stub_46),
            (47, isr_stub_47),
            (48, isr_stub_48),
            (255, isr_stub_255),
        ];

        for (vec, stub) in stubs {
            IDT.0[vec].set_handler(stub as *const () as usize);
        }

        let idtr = DescriptorTablePointer {
            limit: (core::mem::size_of::<Idt>() - 1) as u16,
            base: (&raw const IDT.0) as u64,
        };

        core::arch::asm!("lidt [{}]", in(reg) &idtr, options(nostack, preserves_flags));
    }
}
