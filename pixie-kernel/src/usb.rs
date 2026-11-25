use alloc::vec::Vec;
use spin::Once;

use crate::hal::NativeHal;
use pixie_drivers::pci::{Bar, scan_pci};
use pixie_drivers::xhci::{SharedXhci, XhciController};

static XHCI_CONTROLLERS: Once<Vec<SharedXhci<NativeHal>>> = Once::new();

/// Scan PCI once for xHCI USB 3.x host controllers and return shared controller handles.
/// Both USB Ethernet (`net`) and USB HID keyboards (`input`) attach to these shared handles
/// without resetting controllers out from under one another.
pub fn controllers() -> &'static [SharedXhci<NativeHal>] {
    XHCI_CONTROLLERS.call_once(|| {
        let mut out = Vec::new();
        for dev in scan_pci(&NativeHal) {
            if dev.class_code == 0x0C
                && dev.subclass == 0x03
                && dev.prog_if == 0x30
                && let Some(Bar::Memory { base, size, .. }) = dev.read_bar(&NativeHal, 0)
            {
                dev.enable_bus_mastering(&NativeHal);
                crate::arch::paging::map_mmio(base, size.max(0x40000));
                match XhciController::new_shared(base, NativeHal) {
                    Ok(ctrl) => out.push(ctrl),
                    Err(e) => log::warn!("xHCI controller at {base:#x} init failed: {e}"),
                }
            }
        }
        out
    })
}
