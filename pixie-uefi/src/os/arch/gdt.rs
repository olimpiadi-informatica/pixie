use core::arch::asm;

#[repr(C, packed)]
struct GdtDescriptor {
    size: u16,
    offset: u64,
}

static GDT: [u64; 3] = [
    0,                     // 0x00: Null
    0x0020_9A00_0000_0000, // 0x08: 64-bit Code (Present, Ring 0, Exec/Read, Long mode)
    0x0000_9200_0000_0000, // 0x10: 64-bit Data (Present, Ring 0, Read/Write)
];

pub unsafe fn init() {
    let desc = GdtDescriptor {
        size: (core::mem::size_of_val(&GDT) - 1) as u16,
        offset: GDT.as_ptr() as u64,
    };

    unsafe {
        asm!(
            "lgdt [{desc}]",
            "push 0x08",
            "lea {tmp}, [2f + rip]",
            "push {tmp}",
            "retfq",
            "2:",
            "mov {data_sel:x}, 0x10",
            "mov ds, {data_sel:x}",
            "mov es, {data_sel:x}",
            "mov fs, {data_sel:x}",
            "mov gs, {data_sel:x}",
            "mov ss, {data_sel:x}",
            desc = in(reg) &desc,
            tmp = out(reg) _,
            data_sel = out(reg) _,
        );
    }
}
