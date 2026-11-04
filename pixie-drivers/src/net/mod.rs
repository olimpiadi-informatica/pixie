pub mod e1000;
pub mod igc;
pub(crate) mod intel;
pub mod rtl;
pub mod usb_net;

pub(crate) const FALLBACK_LOCAL_MAC: [u8; 6] = [0x02, b'P', b'I', b'X', b'I', b'E'];

#[inline]
pub(crate) fn is_valid_mac(m: &[u8; 6]) -> bool {
    *m != [0; 6] && *m != [0xFF; 6] && (m[0] & 1) == 0
}

#[inline]
pub(crate) fn resolve_mac(hw_mac: [u8; 6], fallback_mac: Option<[u8; 6]>) -> [u8; 6] {
    if is_valid_mac(&hw_mac) {
        hw_mac
    } else if let Some(fb) = fallback_mac
        && is_valid_mac(&fb)
    {
        fb
    } else {
        FALLBACK_LOCAL_MAC
    }
}
