use core::arch::naked_asm;
use core::sync::atomic::{AtomicU64, Ordering};

use super::apic;

#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    type_attr: u8,
    offset_mid: u16,
    offset_high: u32,
    zero: u32,
}

impl IdtEntry {
    pub const fn empty() -> Self {
        Self {
            offset_low: 0,
            selector: 0,
            ist: 0,
            type_attr: 0,
            offset_mid: 0,
            offset_high: 0,
            zero: 0,
        }
    }

    pub fn set_handler(&mut self, handler: unsafe extern "C" fn()) {
        let addr = handler as usize as u64;
        self.offset_low = addr as u16;
        self.selector = 0x08; // 64-bit Kernel Code Segment
        self.ist = 0;
        self.type_attr = 0x8E; // Present, Ring 0, 64-bit Interrupt Gate
        self.offset_mid = (addr >> 16) as u16;
        self.offset_high = (addr >> 32) as u32;
        self.zero = 0;
    }
}

#[repr(C, packed)]
struct IdtDescriptor {
    size: u16,
    offset: u64,
}

static mut IDT: [IdtEntry; 256] = [IdtEntry::empty(); 256];

/// Bitmask of pending IRQ events for vectors 32..95
pub static IRQ_FLAGS: AtomicU64 = AtomicU64::new(0);

#[unsafe(no_mangle)]
pub extern "C" fn irq_c_handler(vector: u64) {
    if vector == 32 {
        crate::os::watchdog::check_hang();
    }
    if (32..96).contains(&vector) {
        IRQ_FLAGS.fetch_or(1 << (vector - 32), Ordering::Release);
    }
    apic::send_eoi();
}

#[unsafe(no_mangle)]
pub extern "C" fn exception_c_handler(vector: u64, error_code: u64, rip: u64) {
    crate::os::panic::handle_exception(vector, error_code, rip);
}

macro_rules! isr_stub {
    ($name:ident, $vec:expr) => {
        #[unsafe(naked)]
        unsafe extern "C" fn $name() {
            naked_asm!(
                "push rax",
                "push rcx",
                "push rdx",
                "push rsi",
                "push rdi",
                "push r8",
                "push r9",
                "push r10",
                "push r11",
                "mov rcx, {vec}",
                "sub rsp, 32",
                "call {handler}",
                "add rsp, 32",
                "pop r11",
                "pop r10",
                "pop r9",
                "pop r8",
                "pop rdi",
                "pop rsi",
                "pop rdx",
                "pop rcx",
                "pop rax",
                "iretq",
                vec = const $vec,
                handler = sym irq_c_handler,
            );
        }
    };
}

macro_rules! exc_stub_no_err {
    ($name:ident, $vec:expr) => {
        #[unsafe(naked)]
        unsafe extern "C" fn $name() {
            naked_asm!(
                "push 0",
                "push rdi",
                "push rsi",
                "push rdx",
                "push rcx",
                "push rax",
                "push r8",
                "push r9",
                "push r10",
                "push r11",
                "mov rcx, {vec}",
                "mov rdx, [rsp + 9 * 8]",
                "mov r8, [rsp + 10 * 8]",
                "sub rsp, 40",
                "call {handler}",
                "add rsp, 40",
                "pop r11",
                "pop r10",
                "pop r9",
                "pop r8",
                "pop rax",
                "pop rcx",
                "pop rdx",
                "pop rsi",
                "pop rdi",
                "add rsp, 8",
                "iretq",
                vec = const $vec,
                handler = sym exception_c_handler,
            );
        }
    };
}

macro_rules! exc_stub_err {
    ($name:ident, $vec:expr) => {
        #[unsafe(naked)]
        unsafe extern "C" fn $name() {
            naked_asm!(
                "push rdi",
                "push rsi",
                "push rdx",
                "push rcx",
                "push rax",
                "push r8",
                "push r9",
                "push r10",
                "push r11",
                "mov rcx, {vec}",
                "mov rdx, [rsp + 9 * 8]",
                "mov r8, [rsp + 10 * 8]",
                "sub rsp, 40",
                "call {handler}",
                "add rsp, 40",
                "pop r11",
                "pop r10",
                "pop r9",
                "pop r8",
                "pop rax",
                "pop rcx",
                "pop rdx",
                "pop rsi",
                "pop rdi",
                "add rsp, 8",
                "iretq",
                vec = const $vec,
                handler = sym exception_c_handler,
            );
        }
    };
}

exc_stub_no_err!(exc0, 0);
exc_stub_no_err!(exc1, 1);
exc_stub_no_err!(exc2, 2);
exc_stub_no_err!(exc3, 3);
exc_stub_no_err!(exc4, 4);
exc_stub_no_err!(exc5, 5);
exc_stub_no_err!(exc6, 6);
exc_stub_no_err!(exc7, 7);
exc_stub_err!(exc8, 8);
exc_stub_no_err!(exc9, 9);
exc_stub_err!(exc10, 10);
exc_stub_err!(exc11, 11);
exc_stub_err!(exc12, 12);
exc_stub_err!(exc13, 13);
exc_stub_err!(exc14, 14);
exc_stub_no_err!(exc15, 15);
exc_stub_no_err!(exc16, 16);
exc_stub_err!(exc17, 17);
exc_stub_no_err!(exc18, 18);
exc_stub_no_err!(exc19, 19);

isr_stub!(irq32, 32); // APIC Timer / Watchdog
isr_stub!(irq33, 33); // Keyboard
isr_stub!(irq34, 34); // Cascade
isr_stub!(irq35, 35); // COM2
isr_stub!(irq36, 36); // COM1
isr_stub!(irq37, 37);
isr_stub!(irq38, 38);
isr_stub!(irq39, 39);
isr_stub!(irq40, 40); // NIC MSI 1
isr_stub!(irq41, 41); // NIC MSI 2
isr_stub!(irq42, 42); // NVMe MSI
isr_stub!(irq43, 43); // SATA AHCI MSI
isr_stub!(irq44, 44);
isr_stub!(irq45, 45);
isr_stub!(irq46, 46);
isr_stub!(irq47, 47);

#[unsafe(naked)]
unsafe extern "C" fn spurious_isr() {
    naked_asm!("iretq");
}

pub unsafe fn init() {
    unsafe {
        IDT[0].set_handler(exc0);
        IDT[1].set_handler(exc1);
        IDT[2].set_handler(exc2);
        IDT[3].set_handler(exc3);
        IDT[4].set_handler(exc4);
        IDT[5].set_handler(exc5);
        IDT[6].set_handler(exc6);
        IDT[7].set_handler(exc7);
        IDT[8].set_handler(exc8);
        IDT[9].set_handler(exc9);
        IDT[10].set_handler(exc10);
        IDT[11].set_handler(exc11);
        IDT[12].set_handler(exc12);
        IDT[13].set_handler(exc13);
        IDT[14].set_handler(exc14);
        IDT[15].set_handler(exc15);
        IDT[16].set_handler(exc16);
        IDT[17].set_handler(exc17);
        IDT[18].set_handler(exc18);
        IDT[19].set_handler(exc19);

        IDT[32].set_handler(irq32);
        IDT[33].set_handler(irq33);
        IDT[34].set_handler(irq34);
        IDT[35].set_handler(irq35);
        IDT[36].set_handler(irq36);
        IDT[37].set_handler(irq37);
        IDT[38].set_handler(irq38);
        IDT[39].set_handler(irq39);
        IDT[40].set_handler(irq40);
        IDT[41].set_handler(irq41);
        IDT[42].set_handler(irq42);
        IDT[43].set_handler(irq43);
        IDT[44].set_handler(irq44);
        IDT[45].set_handler(irq45);
        IDT[46].set_handler(irq46);
        IDT[47].set_handler(irq47);

        IDT[255].set_handler(spurious_isr);

        let desc = IdtDescriptor {
            size: (core::mem::size_of::<[IdtEntry; 256]>() - 1) as u16,
            offset: (&raw const IDT) as u64,
        };

        core::arch::asm!("lidt [{desc}]", desc = in(reg) &desc);
    }
}
