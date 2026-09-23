pub mod apic;
pub mod gdt;
pub mod idt;
pub mod io;
pub mod paging;

pub unsafe fn init() {
    unsafe {
        gdt::init();
        idt::init();
        apic::init();
    }
}
