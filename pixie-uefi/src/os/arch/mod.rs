pub mod apic;
pub mod gdt;
pub mod idt;
pub mod io;
pub mod paging;

#[allow(dead_code)]
pub unsafe fn init() {
    unsafe {
        gdt::init();
        idt::init();
        apic::init();
    }
}
