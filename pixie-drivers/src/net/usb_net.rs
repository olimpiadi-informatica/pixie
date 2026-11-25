//! USB Ethernet Dongle driver over xHCI (Realtek RTL8153, CDC-ECM, and CDC-NCM).
//!
//! Attaches to a shared [`SharedXhci`] host controller so it can coexist with
//! USB HID keyboards or additional dongles on the same xHCI controller.

use core::mem::ManuallyDrop;
use core::sync::atomic::{Ordering, fence};

use crate::error::{Error, Result};
use crate::hal::{DmaBuffer, Hal};
use crate::net::{is_valid_mac, resolve_mac};
use crate::xhci::{
    CC_BABBLE_DETECTED, CC_DATA_BUFFER_ERROR, CC_SHORT_PACKET, CC_STALL_ERROR, CC_SUCCESS,
    CC_USB_TX_ERROR, CTRL_BUF_SIZE, DEV_OFF_EP_RING, EP_TYPE_BULK_IN, EP_TYPE_BULK_OUT,
    EndpointConfig, RING_SIZE, RingState, SharedXhci, XhciController, endpoint_dci, spin_wait_us,
};

pub const BULK_BUFFER_SIZE: usize = 2048;
const NUM_RX_BUFS: usize = 4;
const NUM_TX_BUFS: usize = 32;

// Per-dongle IO DMA layout (`IO_DMA_PAGES = 19` = 76 KiB)
// 0x00000..0x01000 : Bulk-OUT Transfer Ring (1024 B = 64 TRBs)
// 0x01000..0x03000 : 4 * 2048 B RX buffers
// 0x03000..0x13000 : 32 * 2048 B TX buffers
const IO_DMA_PAGES: usize = 19;
const IO_OFF_BULK_OUT_RING: usize = 0x0000;
const IO_OFF_RX_BUFS: usize = 0x1000;
const IO_OFF_TX_BUFS: usize = 0x3000;

// Realtek RTL8152/RTL8153/RTL8156 framing constants (24-byte RX descriptor, 8-byte TX descriptor)
const RTL_TX_DESC_LEN: usize = 8;
pub const RTL_RX_DESC_LEN: usize = 24;
const RTL_TX_FS: u32 = 1 << 31;
const RTL_TX_LS: u32 = 1 << 30;
const RTL_RX_LEN_MASK: usize = 0x7FFF;
const RTL_FCS_LEN: usize = 4;

// Realtek Vendor ID & PLA request codes
const USB_VID_REALTEK: u16 = 0x0BDA;
const RTL_REQ_REGS: u8 = 0x05;
const RTL_MCU_TYPE_PLA: u16 = 0x0100;
const RTL_PLA_IDR: u16 = 0xC000;
const RTL_PLA_RCR: u16 = 0xC010;
const RTL_PLA_CR: u16 = 0xE813;

// USB CDC NCM signatures
const NTH16_SIG: &[u8; 4] = b"NCMH";
const NDP16_SIG_NO_CRC: &[u8; 4] = b"NCM0";
const NDP16_SIG_CRC: &[u8; 4] = b"NCM1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsbNetProtocol {
    Rtl8153,
    CdcEcm,
    CdcNcm,
}

/// Encode a raw Ethernet `frame` into a single RTL8153 Bulk-OUT transfer (8-byte TX descriptor + frame).
pub fn encode_rtl8153_tx_frame(frame: &[u8], out: &mut [u8]) -> usize {
    if frame.is_empty() || out.len() < RTL_TX_DESC_LEN + frame.len() {
        return 0;
    }
    let opts1 = RTL_TX_FS | RTL_TX_LS | ((frame.len() as u32) & 0xFFFF);
    out[0..4].copy_from_slice(&opts1.to_le_bytes());
    out[4..8].copy_from_slice(&0u32.to_le_bytes());
    out[8..8 + frame.len()].copy_from_slice(frame);
    RTL_TX_DESC_LEN + frame.len()
}

/// Decode the first Ethernet frame from an RTL8153 Bulk-IN transfer (24-byte RX descriptor + frame + 4-byte FCS).
pub fn decode_rtl8153_rx_frame(transfer: &[u8]) -> Option<&[u8]> {
    if transfer.len() < RTL_RX_DESC_LEN + RTL_FCS_LEN + 1 {
        return None;
    }
    let opts1 = u32::from_le_bytes(transfer[0..4].try_into().ok()?);
    let raw_len = (opts1 as usize) & RTL_RX_LEN_MASK;
    if raw_len <= RTL_FCS_LEN {
        return None;
    }
    let end = RTL_RX_DESC_LEN.checked_add(raw_len)?;
    if end > transfer.len() {
        return None;
    }
    let frame_len = raw_len - RTL_FCS_LEN;
    Some(&transfer[RTL_RX_DESC_LEN..RTL_RX_DESC_LEN + frame_len])
}

/// Iterate across aggregated 8-byte aligned RTL8153 RX frames at `*cursor`, advancing `*cursor`
/// across valid frames until exhausted.
fn next_rtl8153_rx_frame<'a>(transfer: &'a [u8], cursor: &mut usize) -> Option<&'a [u8]> {
    if *cursor + RTL_RX_DESC_LEN <= transfer.len() {
        let start = *cursor;
        let opts1 = u32::from_le_bytes(transfer[start..start + 4].try_into().ok()?);
        let raw_len = (opts1 as usize) & RTL_RX_LEN_MASK;
        if raw_len <= RTL_FCS_LEN {
            *cursor = transfer.len();
            return None;
        }
        let end = start.checked_add(RTL_RX_DESC_LEN)?.checked_add(raw_len)?;
        if end > transfer.len() {
            *cursor = transfer.len();
            return None;
        }
        // Next aggregated descriptor starts at 8-byte alignment
        let consumed = RTL_RX_DESC_LEN + raw_len;
        *cursor = start + ((consumed + 7) & !7);
        return Some(&transfer[start + RTL_RX_DESC_LEN..end - RTL_FCS_LEN]);
    }
    None
}

/// Encode a single Ethernet `frame` into a CDC-NCM NTB16 Bulk-OUT transfer (12B NTH16 + 16B NDP16 + frame).
pub fn encode_cdc_ncm_tx_frame(seq: u16, frame: &[u8], out: &mut [u8]) -> usize {
    if frame.is_empty() {
        return 0;
    }
    let ndp_off = 12usize;
    let ndp_len = 16usize;
    let frame_off = (ndp_off + ndp_len + 3) & !3; // 32 (4-byte aligned >= 28)
    let block_len = frame_off + frame.len();
    if out.len() < block_len {
        return 0;
    }

    // NTH16 (12 bytes)
    out[0..4].copy_from_slice(NTH16_SIG);
    out[4..6].copy_from_slice(&12u16.to_le_bytes()); // wHeaderLength
    out[6..8].copy_from_slice(&seq.to_le_bytes()); // wSequence
    out[8..10].copy_from_slice(&(block_len as u16).to_le_bytes()); // wBlockLength
    out[10..12].copy_from_slice(&(ndp_off as u16).to_le_bytes()); // wNdpIndex

    // NDP16 NCM0 (16 bytes: 8B header + 4B DPE0 + 4B zero terminator)
    out[ndp_off..ndp_off + 4].copy_from_slice(NDP16_SIG_NO_CRC);
    out[ndp_off + 4..ndp_off + 6].copy_from_slice(&(ndp_len as u16).to_le_bytes());
    out[ndp_off + 6..ndp_off + 8].copy_from_slice(&0u16.to_le_bytes()); // wNextNdpIndex
    out[ndp_off + 8..ndp_off + 10].copy_from_slice(&(frame_off as u16).to_le_bytes());
    out[ndp_off + 10..ndp_off + 12].copy_from_slice(&(frame.len() as u16).to_le_bytes());
    out[ndp_off + 12..ndp_off + 16].fill(0);

    if frame_off > ndp_off + ndp_len {
        out[ndp_off + ndp_len..frame_off].fill(0);
    }
    out[frame_off..block_len].copy_from_slice(frame);
    block_len
}

/// Decode the first Ethernet datagram from a CDC-NCM NTB16 Bulk-IN transfer.
pub fn decode_cdc_ncm_rx_frame(ntb: &[u8]) -> Option<&[u8]> {
    let mut ndp_off = 0usize;
    let mut item_idx = 0usize;
    next_cdc_ncm_rx_frame(ntb, &mut ndp_off, &mut item_idx)
}

fn next_cdc_ncm_rx_frame<'a>(
    ntb: &'a [u8],
    ndp_off: &mut usize,
    item_idx: &mut usize,
) -> Option<&'a [u8]> {
    if ntb.len() < 12 || &ntb[0..4] != NTH16_SIG {
        return None;
    }
    let hdr_len = u16::from_le_bytes(ntb[4..6].try_into().ok()?) as usize;
    if hdr_len != 12 {
        return None;
    }
    let block_len = u16::from_le_bytes(ntb[8..10].try_into().ok()?) as usize;
    if block_len < 12 || block_len > ntb.len() {
        return None;
    }
    if *ndp_off == 0 {
        let first_ndp = u16::from_le_bytes(ntb[10..12].try_into().ok()?) as usize;
        if first_ndp < 12 || first_ndp + 8 > block_len {
            return None;
        }
        *ndp_off = first_ndp;
        *item_idx = 0;
    }

    for _hop in 0..16 {
        let off = *ndp_off;
        if off == 0 || off + 8 > block_len {
            return None;
        }
        let sig = &ntb[off..off + 4];
        let has_crc = if sig == NDP16_SIG_NO_CRC {
            false
        } else if sig == NDP16_SIG_CRC {
            true
        } else {
            return None;
        };
        let ndp_len = u16::from_le_bytes(ntb[off + 4..off + 6].try_into().ok()?) as usize;
        if ndp_len < 12 || off + ndp_len > block_len {
            return None;
        }
        let next_ndp = u16::from_le_bytes(ntb[off + 6..off + 8].try_into().ok()?) as usize;

        loop {
            let entry_off = off + 8 + (*item_idx) * 4;
            if entry_off + 4 > off + ndp_len {
                break;
            }
            *item_idx += 1;
            let dg_idx =
                u16::from_le_bytes(ntb[entry_off..entry_off + 2].try_into().ok()?) as usize;
            let dg_len =
                u16::from_le_bytes(ntb[entry_off + 2..entry_off + 4].try_into().ok()?) as usize;
            if dg_idx == 0 && dg_len == 0 {
                break;
            }
            if dg_idx + dg_len > block_len {
                continue;
            }
            let payload_len = if has_crc {
                dg_len.checked_sub(4)?
            } else {
                dg_len
            };
            if payload_len > 0 {
                return Some(&ntb[dg_idx..dg_idx + payload_len]);
            }
        }

        if next_ndp == 0 || next_ndp <= off {
            return None;
        }
        *ndp_off = next_ndp;
        *item_idx = 0;
    }
    None
}

#[derive(Clone, Copy)]
struct UsbNetCfgMatch {
    protocol: UsbNetProtocol,
    config_val: u8,
    comm_iface: u8,
    data_iface: u8,
    data_alt: u8,
    bulk_in_ep: u8,
    bulk_in_mps: u16,
    bulk_out_ep: u8,
    bulk_out_mps: u16,
    i_mac_idx: u8,
}

fn parse_usb_net_config(cfg_buf: &[u8], vid: u16) -> Option<UsbNetCfgMatch> {
    if cfg_buf.len() < 9 || cfg_buf[1] != 2 {
        return None;
    }
    let total_len = (u16::from_le_bytes([cfg_buf[2], cfg_buf[3]]) as usize).min(cfg_buf.len());
    let config_val = cfg_buf[5].max(1);

    let mut cdc_proto: Option<UsbNetProtocol> = None;
    let mut comm_iface: u8 = 0;
    let mut union_data_iface: Option<u8> = None;
    let mut i_mac_idx: u8 = 0;

    #[derive(Clone, Copy)]
    struct DataAltCandidate {
        iface: u8,
        alt: u8,
        in_ep: u8,
        in_mps: u16,
        out_ep: u8,
        out_mps: u16,
        score: u8,
    }
    let mut best_data: Option<DataAltCandidate> = None;
    let mut best_rtl: Option<DataAltCandidate> = None;

    let mut cur_iface: u8 = 0;
    let mut cur_alt: u8 = 0;
    let mut cur_class: u8 = 0;
    let mut cur_subclass: u8 = 0;
    let mut cur_in: Option<(u8, u16)> = None;
    let mut cur_out: Option<(u8, u16)> = None;

    let flush_cur = |cur_iface: u8,
                     cur_alt: u8,
                     cur_class: u8,
                     cur_subclass: u8,
                     cur_in: Option<(u8, u16)>,
                     cur_out: Option<(u8, u16)>,
                     union_data_iface: Option<u8>,
                     best_data: &mut Option<DataAltCandidate>,
                     best_rtl: &mut Option<DataAltCandidate>| {
        let (Some((in_ep, in_mps)), Some((out_ep, out_mps))) = (cur_in, cur_out) else {
            return;
        };
        if cur_class == 0x0A {
            if let Some(target) = union_data_iface
                && target != cur_iface
            {
                return;
            }
            let score = if union_data_iface == Some(cur_iface) {
                4
            } else {
                2
            } + if cur_alt != 0 { 1 } else { 0 };
            if best_data.is_none_or(|b| score > b.score) {
                *best_data = Some(DataAltCandidate {
                    iface: cur_iface,
                    alt: cur_alt,
                    in_ep,
                    in_mps,
                    out_ep,
                    out_mps,
                    score,
                });
            }
        } else if cur_class == 0x02 && (cur_subclass == 0x06 || cur_subclass == 0x0D) {
            // Single-interface CDC-ECM/NCM variant with Bulk IN/OUT directly on Comm interface
            let score = 1 + if cur_alt != 0 { 1 } else { 0 };
            if best_data.is_none_or(|b| score > b.score) {
                *best_data = Some(DataAltCandidate {
                    iface: cur_iface,
                    alt: cur_alt,
                    in_ep,
                    in_mps,
                    out_ep,
                    out_mps,
                    score,
                });
            }
        } else if cur_class == 0xFF && vid == USB_VID_REALTEK {
            let score = if cur_alt != 0 { 2 } else { 1 };
            if best_rtl.is_none_or(|b| score > b.score) {
                *best_rtl = Some(DataAltCandidate {
                    iface: cur_iface,
                    alt: cur_alt,
                    in_ep,
                    in_mps,
                    out_ep,
                    out_mps,
                    score,
                });
            }
        }
    };

    let mut off = 0usize;
    while off + 2 <= total_len {
        let len = cfg_buf[off] as usize;
        let dtype = cfg_buf[off + 1];
        if len < 2 || off + len > total_len {
            break;
        }
        match dtype {
            4 if len >= 9 => {
                flush_cur(
                    cur_iface,
                    cur_alt,
                    cur_class,
                    cur_subclass,
                    cur_in,
                    cur_out,
                    union_data_iface,
                    &mut best_data,
                    &mut best_rtl,
                );
                cur_iface = cfg_buf[off + 2];
                cur_alt = cfg_buf[off + 3];
                cur_class = cfg_buf[off + 5];
                cur_subclass = cfg_buf[off + 6];
                cur_in = None;
                cur_out = None;

                if cur_class == 0x02 && cur_subclass == 0x0D {
                    cdc_proto = Some(UsbNetProtocol::CdcNcm);
                    comm_iface = cur_iface;
                } else if cur_class == 0x02 && cur_subclass == 0x06 && cdc_proto.is_none() {
                    cdc_proto = Some(UsbNetProtocol::CdcEcm);
                    comm_iface = cur_iface;
                }
            }
            0x24 if len >= 4 && cur_class == 0x02 => {
                let subtype = cfg_buf[off + 2];
                match subtype {
                    0x06 if len >= 5 => {
                        // CDC Union Functional Descriptor: bControlInterface, bSubordinateInterface0
                        if cfg_buf[off + 3] == comm_iface {
                            union_data_iface = Some(cfg_buf[off + 4]);
                        }
                    }
                    0x0F if len >= 4 => {
                        // CDC Ethernet Networking Functional Descriptor: iMACAddress
                        i_mac_idx = cfg_buf[off + 3];
                    }
                    _ => {}
                }
            }
            5 if len >= 7 => {
                let ep_addr = cfg_buf[off + 2];
                let bm_attr = cfg_buf[off + 3];
                if (bm_attr & 0x03) == 2 {
                    let mps = (u16::from_le_bytes([cfg_buf[off + 4], cfg_buf[off + 5]]) & 0x07FF)
                        .clamp(64, 1024);
                    if (ep_addr & 0x80) != 0 {
                        if cur_in.is_none() {
                            cur_in = Some((ep_addr, mps));
                        }
                    } else if cur_out.is_none() {
                        cur_out = Some((ep_addr, mps));
                    }
                }
            }
            _ => {}
        }
        off += len;
    }
    flush_cur(
        cur_iface,
        cur_alt,
        cur_class,
        cur_subclass,
        cur_in,
        cur_out,
        union_data_iface,
        &mut best_data,
        &mut best_rtl,
    );

    if let (Some(proto), Some(d)) = (cdc_proto, best_data) {
        return Some(UsbNetCfgMatch {
            protocol: proto,
            config_val,
            comm_iface,
            data_iface: d.iface,
            data_alt: d.alt,
            bulk_in_ep: d.in_ep,
            bulk_in_mps: d.in_mps,
            bulk_out_ep: d.out_ep,
            bulk_out_mps: d.out_mps,
            i_mac_idx,
        });
    }
    if let Some(d) = best_rtl {
        return Some(UsbNetCfgMatch {
            protocol: UsbNetProtocol::Rtl8153,
            config_val,
            comm_iface: d.iface,
            data_iface: d.iface,
            data_alt: d.alt,
            bulk_in_ep: d.in_ep,
            bulk_in_mps: d.in_mps,
            bulk_out_ep: d.out_ep,
            bulk_out_mps: d.out_mps,
            i_mac_idx: 0,
        });
    }
    None
}

fn parse_usb_mac_string_desc(desc: &[u8]) -> Option<[u8; 6]> {
    if desc.len() < 26 || desc[1] != 3 {
        return None;
    }
    let hex_val = |ch: u16| -> Option<u8> {
        match ch {
            0x30..=0x39 => Some((ch - 0x30) as u8),
            0x41..=0x46 => Some((ch - 0x41 + 10) as u8),
            0x61..=0x66 => Some((ch - 0x61 + 10) as u8),
            _ => None,
        }
    };
    let mut mac = [0u8; 6];
    let mut nibble_count = 0usize;
    let mut off = 2usize;
    while off + 2 <= desc.len() && nibble_count < 12 {
        let code = u16::from_le_bytes([desc[off], desc[off + 1]]);
        off += 2;
        if let Some(n) = hex_val(code) {
            let byte_idx = nibble_count / 2;
            if nibble_count.is_multiple_of(2) {
                mac[byte_idx] = n << 4;
            } else {
                mac[byte_idx] |= n;
            }
            nibble_count += 1;
        }
    }
    if nibble_count == 12 && is_valid_mac(&mac) {
        Some(mac)
    } else {
        None
    }
}

struct UsbNetInner {
    slot_dma: ManuallyDrop<DmaBuffer>,
    io_dma: ManuallyDrop<DmaBuffer>,
    ep0_ring: RingState,
    bulk_in_ring: RingState,
    bulk_out_ring: RingState,
    rx_trb_idx: [u16; NUM_RX_BUFS],
    rx_ready_buf: [u8; NUM_RX_BUFS],
    rx_ready_len: [u16; NUM_RX_BUFS],
    mac: [u8; 6],
    tx_zlp_mask: u32,
    slot_id: u8,
    port_num: u8,
    protocol: UsbNetProtocol,
    bulk_in_dci: u8,
    bulk_out_dci: u8,
    bulk_out_mps: u16,
    ncm_seq: u16,
    tx_head: u8,
    tx_tail: u8,
    tx_in_flight: u8,
    tx_trbs_in_flight: u8,
    rx_head: u8,
    rx_ready_head: u8,
    rx_ready_tail: u8,
    rx_ready_count: u8,
    active_rx_buf: u8,
    active_rx_len: u16,
    active_rx_cursor: u16,
    active_ncm_ndp: u16,
    active_ncm_idx: u16,
}

/// Polling-mode USB Ethernet Dongle driver supporting Realtek RTL8153, CDC-ECM, and CDC-NCM.
pub struct UsbEthernetDevice<H: Hal> {
    ctrl: SharedXhci<H>,
    inner: UsbNetInner,
}

// Keep struct footprint compact for kernel driver wrappers.
#[allow(dead_code)]
struct SizeCheckHal;
impl Hal for SizeCheckHal {
    fn mmio_read8(&self, _: u64) -> u8 {
        0
    }
    fn mmio_write8(&self, _: u64, _: u8) {}
    fn mmio_read16(&self, _: u64) -> u16 {
        0
    }
    fn mmio_write16(&self, _: u64, _: u16) {}
    fn mmio_read32(&self, _: u64) -> u32 {
        0
    }
    fn mmio_write32(&self, _: u64, _: u32) {}
    fn mmio_read64(&self, _: u64) -> u64 {
        0
    }
    fn mmio_write64(&self, _: u64, _: u64) {}
    fn io_read8(&self, _: u16) -> u8 {
        0
    }
    fn io_write8(&self, _: u16, _: u8) {}
    fn io_read16(&self, _: u16) -> u16 {
        0
    }
    fn io_write16(&self, _: u16, _: u16) {}
    fn io_read32(&self, _: u16) -> u32 {
        0
    }
    fn io_write32(&self, _: u16, _: u32) {}
    fn alloc_dma(&self, _: usize) -> Option<DmaBuffer> {
        None
    }
    fn free_dma(&self, _: DmaBuffer) {}
    fn micros(&self) -> i64 {
        0
    }
    fn yield_now(&self) -> impl core::future::Future<Output = ()> {
        core::future::ready(())
    }
    fn sleep(&self, _: core::time::Duration) -> impl core::future::Future<Output = ()> {
        core::future::ready(())
    }
}
const _: () = assert!(core::mem::size_of::<UsbEthernetDevice<SizeCheckHal>>() <= 192);

unsafe impl<H: Hal + Send> Send for UsbEthernetDevice<H> {}

impl<H: Hal> UsbEthernetDevice<H> {
    /// Initialize a standalone xHCI host controller at `mmio_base` and enumerate a USB NIC.
    pub fn new(mmio_base: u64, fallback_mac: Option<[u8; 6]>, hal: H) -> Result<Self> {
        let ctrl = XhciController::new_shared(mmio_base, hal)?;
        Self::attach(ctrl, fallback_mac)
    }

    /// Attach a USB Ethernet Dongle on a shared xHCI host controller.
    pub fn attach(ctrl: SharedXhci<H>, fallback_mac: Option<[u8; 6]>) -> Result<Self> {
        let mut g = ctrl
            .try_lock()
            .ok_or(Error::msg("xHCI controller lock busy"))?;
        let max_ports = g.caps().max_ports;
        for port_num in 1..=max_ports {
            if g.is_port_claimed(port_num) {
                continue;
            }
            if let Some(dev) = Self::try_probe_port(&mut g, &ctrl, port_num, fallback_mac)? {
                return Ok(dev);
            }
        }
        Err(Error::msg("No USB Ethernet dongle found on xHCI ports"))
    }

    pub fn protocol(&self) -> UsbNetProtocol {
        self.inner.protocol
    }

    pub fn mac_address(&self) -> [u8; 6] {
        self.inner.mac
    }

    fn try_probe_port(
        g: &mut XhciController<H>,
        ctrl_arc: &SharedXhci<H>,
        port_num: u8,
        fallback_mac: Option<[u8; 6]>,
    ) -> Result<Option<Self>> {
        let Ok(speed) = g.ensure_port_ready(port_num) else {
            return Ok(None);
        };
        let Ok(slot_id) = g.enable_slot() else {
            return Ok(None);
        };
        let mut slot_dma = match g.alloc_slot_dma() {
            Ok(d) => d,
            Err(e) => {
                let _ = g.disable_slot(slot_id);
                return Err(e);
            }
        };
        let mut ep0_ring = RingState::new();
        if g.address_device(slot_id, port_num, speed, &mut slot_dma, &mut ep0_ring)
            .is_err()
        {
            let _ = g.disable_slot(slot_id);
            g.hal().free_dma(slot_dma);
            return Ok(None);
        }

        if speed == 1 {
            let mut dev8 = [0u8; 8];
            let setup_dev8 = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 0x08, 0x00];
            if let Ok(n) =
                g.control_in(slot_id, &mut slot_dma, &mut ep0_ring, setup_dev8, &mut dev8)
                && n >= 8
                && dev8[7] != 8
                && dev8[7] >= 8
            {
                let _ = g.evaluate_ep0_mps(slot_id, dev8[7] as u16, &slot_dma);
            }
        }

        // Read full Device Descriptor to inspect VID & bNumConfigurations
        let mut dev_desc = [0u8; 18];
        let setup_dev = [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 18, 0x00];
        let Ok(dev_n) = g.control_in(
            slot_id,
            &mut slot_dma,
            &mut ep0_ring,
            setup_dev,
            &mut dev_desc,
        ) else {
            let _ = g.disable_slot(slot_id);
            g.hal().free_dma(slot_dma);
            return Ok(None);
        };
        let (vid, num_cfgs) = if dev_n >= 18 && dev_desc[1] == 1 {
            (
                u16::from_le_bytes([dev_desc[8], dev_desc[9]]),
                dev_desc[17].clamp(1, 8),
            )
        } else {
            (0, 1)
        };

        // Prefer CDC-NCM/CDC-ECM configuration if exposed alongside vendor mode (e.g. RTL8153 cfg 2),
        // otherwise select Realtek RTL8153 vendor config.
        let mut best_cdc: Option<UsbNetCfgMatch> = None;
        let mut best_rtl: Option<UsbNetCfgMatch> = None;
        let mut cfg_buf = [0u8; CTRL_BUF_SIZE];

        for cfg_idx in 0..num_cfgs {
            let setup_hdr = [0x80, 0x06, cfg_idx, 0x02, 0x00, 0x00, 9, 0x00];
            let Ok(n) = g.control_in(
                slot_id,
                &mut slot_dma,
                &mut ep0_ring,
                setup_hdr,
                &mut cfg_buf[..9],
            ) else {
                continue;
            };
            if n < 9 || cfg_buf[1] != 2 {
                continue;
            }
            let total_len =
                (u16::from_le_bytes([cfg_buf[2], cfg_buf[3]]) as usize).clamp(9, CTRL_BUF_SIZE);
            let parsed_len = if total_len > 9 {
                let len_le = (total_len as u16).to_le_bytes();
                let setup_full = [0x80, 0x06, cfg_idx, 0x02, 0x00, 0x00, len_le[0], len_le[1]];
                let Ok(full_n) = g.control_in(
                    slot_id,
                    &mut slot_dma,
                    &mut ep0_ring,
                    setup_full,
                    &mut cfg_buf[..total_len],
                ) else {
                    continue;
                };
                full_n.min(total_len)
            } else {
                n
            };
            if let Some(m) = parse_usb_net_config(&cfg_buf[..parsed_len], vid) {
                match m.protocol {
                    UsbNetProtocol::CdcNcm => {
                        best_cdc = Some(m);
                        break;
                    }
                    UsbNetProtocol::CdcEcm => {
                        if best_cdc.is_none() {
                            best_cdc = Some(m);
                        }
                    }
                    UsbNetProtocol::Rtl8153 => {
                        if best_rtl.is_none() {
                            best_rtl = Some(m);
                        }
                    }
                }
            }
        }

        let Some(cfg) = best_cdc.or(best_rtl) else {
            let _ = g.disable_slot(slot_id);
            g.hal().free_dma(slot_dma);
            return Ok(None);
        };

        // SET_CONFIGURATION(cfg.config_val)
        let setup_cfg = [0x00, 0x09, cfg.config_val, 0x00, 0x00, 0x00, 0x00, 0x00];
        if g.control_out(slot_id, &mut slot_dma, &mut ep0_ring, setup_cfg, &[])
            .is_err()
        {
            let _ = g.disable_slot(slot_id);
            g.hal().free_dma(slot_dma);
            return Ok(None);
        }

        // SET_INTERFACE for alternate setting on CDC Data interface (when alt != 0)
        if cfg.data_alt != 0 {
            let setup_iface = [
                0x01,
                0x0B,
                cfg.data_alt,
                0x00,
                cfg.data_iface,
                0x00,
                0x00,
                0x00,
            ];
            let _ = g.control_out(slot_id, &mut slot_dma, &mut ep0_ring, setup_iface, &[]);
        }

        // Resolve MAC address
        let mut hw_mac = [0u8; 6];
        match cfg.protocol {
            UsbNetProtocol::Rtl8153 => {
                // Read RTL8153 MAC from PLA IDR registers (0xC000)
                let idx_le = RTL_MCU_TYPE_PLA.to_le_bytes();
                let addr_le = RTL_PLA_IDR.to_le_bytes();
                let setup_mac = [
                    0xC0,
                    RTL_REQ_REGS,
                    addr_le[0],
                    addr_le[1],
                    idx_le[0],
                    idx_le[1],
                    6,
                    0,
                ];
                let _ = g.control_in(
                    slot_id,
                    &mut slot_dma,
                    &mut ep0_ring,
                    setup_mac,
                    &mut hw_mac,
                );
                // Best-effort RTL8153 RX/TX & packet filter enable for vendor mode hardware
                Self::rtl8153_enable_pla(g, slot_id, &mut slot_dma, &mut ep0_ring);
            }
            UsbNetProtocol::CdcEcm | UsbNetProtocol::CdcNcm => {
                if cfg.i_mac_idx != 0 {
                    let mut str_buf = [0u8; 64];
                    let setup_str = [0x80, 0x06, cfg.i_mac_idx, 0x03, 0x09, 0x04, 64, 0x00];
                    if let Ok(n) = g.control_in(
                        slot_id,
                        &mut slot_dma,
                        &mut ep0_ring,
                        setup_str,
                        &mut str_buf,
                    ) && let Some(parsed) = parse_usb_mac_string_desc(&str_buf[..n])
                    {
                        hw_mac = parsed;
                    }
                }
                // Enable Directed + All-Multicast + Broadcast packet reception
                let setup_filter = [0x21, 0x43, 0x0E, 0x00, cfg.comm_iface, 0x00, 0x00, 0x00];
                let _ = g.control_out(slot_id, &mut slot_dma, &mut ep0_ring, setup_filter, &[]);

                if cfg.protocol == UsbNetProtocol::CdcNcm {
                    let mut params = [0u8; 28];
                    let setup_get_ntb = [0xA1, 0x80, 0x00, 0x00, cfg.comm_iface, 0x00, 28, 0x00];
                    let _ = g.control_in(
                        slot_id,
                        &mut slot_dma,
                        &mut ep0_ring,
                        setup_get_ntb,
                        &mut params,
                    );
                    let in_size = (BULK_BUFFER_SIZE as u32).to_le_bytes();
                    let setup_set_ntb = [0x21, 0x86, 0x00, 0x00, cfg.comm_iface, 0x00, 4, 0x00];
                    let _ = g.control_out(
                        slot_id,
                        &mut slot_dma,
                        &mut ep0_ring,
                        setup_set_ntb,
                        &in_size,
                    );
                }
            }
        }

        let mac = resolve_mac(hw_mac, fallback_mac);

        // Allocate IO DMA (Bulk-OUT ring + TX buffer + 4 x 2048B RX buffers)
        let Some(io_dma) = g.hal().alloc_dma(IO_DMA_PAGES) else {
            let _ = g.disable_slot(slot_id);
            g.hal().free_dma(slot_dma);
            return Err(Error::msg("Failed to allocate USB Ethernet IO DMA"));
        };
        if !g.check_dma_bounds(&io_dma, IO_DMA_PAGES) {
            g.hal().free_dma(io_dma);
            let _ = g.disable_slot(slot_id);
            g.hal().free_dma(slot_dma);
            return Err(Error::msg("USB Ethernet IO DMA exceeds 32-bit limit"));
        }
        unsafe {
            core::ptr::write_bytes(io_dma.virt, 0, IO_DMA_PAGES * 4096);
        }

        let bulk_in_dci = endpoint_dci(cfg.bulk_in_ep);
        let bulk_out_dci = endpoint_dci(cfg.bulk_out_ep);

        let mut bulk_in_ring = RingState::new();
        let bulk_in_virt = unsafe { slot_dma.virt.add(DEV_OFF_EP_RING) };
        let bulk_in_phys = slot_dma.phys + DEV_OFF_EP_RING as u64;
        g.init_transfer_ring(bulk_in_virt, bulk_in_phys, &mut bulk_in_ring);

        let mut bulk_out_ring = RingState::new();
        let bulk_out_virt = unsafe { io_dma.virt.add(IO_OFF_BULK_OUT_RING) };
        let bulk_out_phys = io_dma.phys + IO_OFF_BULK_OUT_RING as u64;
        g.init_transfer_ring(bulk_out_virt, bulk_out_phys, &mut bulk_out_ring);

        let ep_cfgs = [
            EndpointConfig {
                ep_dci: bulk_in_dci,
                ep_type: EP_TYPE_BULK_IN,
                max_packet_size: cfg.bulk_in_mps,
                max_burst: 0,
                interval: 0,
                avg_trb_len: cfg.bulk_in_mps,
                max_esit_payload_lo: 0,
                ring_phys: bulk_in_phys,
            },
            EndpointConfig {
                ep_dci: bulk_out_dci,
                ep_type: EP_TYPE_BULK_OUT,
                max_packet_size: cfg.bulk_out_mps,
                max_burst: 0,
                interval: 0,
                avg_trb_len: cfg.bulk_out_mps,
                max_esit_payload_lo: 0,
                ring_phys: bulk_out_phys,
            },
        ];
        if g.configure_endpoints(slot_id, &slot_dma, &ep_cfgs).is_err() {
            g.hal().free_dma(io_dma);
            let _ = g.disable_slot(slot_id);
            g.hal().free_dma(slot_dma);
            return Ok(None);
        }

        // Pre-submit NUM_RX_BUFS Bulk-IN TRBs
        let mut rx_trb_idx = [0u16; NUM_RX_BUFS];
        for (i, slot) in rx_trb_idx.iter_mut().enumerate() {
            let trb_idx = bulk_in_ring.index;
            *slot = trb_idx;
            let buf_phys = io_dma.phys + (IO_OFF_RX_BUFS + i * BULK_BUFFER_SIZE) as u64;
            g.enqueue_normal(
                bulk_in_virt,
                bulk_in_phys,
                &mut bulk_in_ring,
                buf_phys,
                BULK_BUFFER_SIZE as u32,
                true,
                true,
            );
        }
        g.ring_doorbell(slot_id, bulk_in_dci as u32);
        let _ = g.claim_port(port_num);

        Ok(Some(Self {
            ctrl: ctrl_arc.clone(),
            inner: UsbNetInner {
                slot_dma: ManuallyDrop::new(slot_dma),
                io_dma: ManuallyDrop::new(io_dma),
                ep0_ring,
                bulk_in_ring,
                bulk_out_ring,
                rx_trb_idx,
                rx_ready_buf: [0; NUM_RX_BUFS],
                rx_ready_len: [0; NUM_RX_BUFS],
                mac,
                tx_zlp_mask: 0,
                slot_id,
                port_num,
                protocol: cfg.protocol,
                bulk_in_dci,
                bulk_out_dci,
                bulk_out_mps: cfg.bulk_out_mps,
                ncm_seq: 0,
                tx_head: 0,
                tx_tail: 0,
                tx_in_flight: 0,
                tx_trbs_in_flight: 0,
                rx_head: 0,
                rx_ready_head: 0,
                rx_ready_tail: 0,
                rx_ready_count: 0,
                active_rx_buf: u8::MAX,
                active_rx_len: 0,
                active_rx_cursor: 0,
                active_ncm_ndp: 0,
                active_ncm_idx: 0,
            },
        }))
    }

    fn rtl8153_enable_pla(
        g: &mut XhciController<H>,
        slot_id: u8,
        slot_dma: &mut DmaBuffer,
        ep0_ring: &mut RingState,
    ) {
        let mut rcr = [0u8; 4];
        let idx_rw = (RTL_MCU_TYPE_PLA | 0x000F).to_le_bytes();
        let rcr_le = RTL_PLA_RCR.to_le_bytes();
        let setup_get_rcr = [0xC0, RTL_REQ_REGS, rcr_le[0], rcr_le[1], 0x00, 0x01, 4, 0];
        if g.control_in(slot_id, slot_dma, ep0_ring, setup_get_rcr, &mut rcr)
            .is_ok()
        {
            let val = u32::from_le_bytes(rcr) | 0x0E; // Accept Physical/Multicast/Broadcast
            let setup_set_rcr = [
                0x40,
                RTL_REQ_REGS,
                rcr_le[0],
                rcr_le[1],
                idx_rw[0],
                idx_rw[1],
                4,
                0,
            ];
            let _ = g.control_out(
                slot_id,
                slot_dma,
                ep0_ring,
                setup_set_rcr,
                &val.to_le_bytes(),
            );
        }

        let cr_le = (RTL_PLA_CR & !3).to_le_bytes();
        let mut cr_buf = [0u8; 4];
        let setup_get_cr = [0xC0, RTL_REQ_REGS, cr_le[0], cr_le[1], 0x00, 0x01, 4, 0];
        if g.control_in(slot_id, slot_dma, ep0_ring, setup_get_cr, &mut cr_buf)
            .is_ok()
        {
            cr_buf[3] |= 0x0C; // RE | TE at 0xE813
            let idx_cr = (RTL_MCU_TYPE_PLA | 0x0080).to_le_bytes();
            let setup_set_cr = [
                0x40,
                RTL_REQ_REGS,
                cr_le[0],
                cr_le[1],
                idx_cr[0],
                idx_cr[1],
                4,
                0,
            ];
            let _ = g.control_out(slot_id, slot_dma, ep0_ring, setup_set_cr, &cr_buf);
        }
    }

    pub fn can_transmit(&mut self) -> bool {
        let inner = &mut self.inner;
        if let Some(mut g) = self.ctrl.try_lock() {
            inner.reclaim_tx_events(&mut g);
        }
        (inner.tx_in_flight as usize) < NUM_TX_BUFS
            && (inner.tx_trbs_in_flight as usize) + 2 <= (RING_SIZE - 1)
    }

    pub fn transmit(&mut self, packet: &[u8]) -> Result<()> {
        if packet.is_empty() || packet.len() > 1600 {
            return Err(Error::msg("Invalid USB Ethernet TX frame length"));
        }

        let inner = &mut self.inner;
        let mut g = self
            .ctrl
            .try_lock()
            .ok_or(Error::msg("xHCI controller lock busy"))?;
        inner.reclaim_tx_events(&mut g);

        if (inner.tx_in_flight as usize) >= NUM_TX_BUFS {
            return Err(Error::msg("USB Ethernet TX queue full"));
        }

        let slot_idx = inner.tx_tail as usize;
        let tx_off = IO_OFF_TX_BUFS + slot_idx * BULK_BUFFER_SIZE;
        let tx_buf = unsafe {
            core::slice::from_raw_parts_mut(inner.io_dma.virt.add(tx_off), BULK_BUFFER_SIZE)
        };
        let wire_len = match inner.protocol {
            UsbNetProtocol::Rtl8153 => encode_rtl8153_tx_frame(packet, tx_buf),
            UsbNetProtocol::CdcEcm => {
                if packet.len() > BULK_BUFFER_SIZE {
                    return Err(Error::msg("CDC-ECM frame exceeds buffer"));
                }
                tx_buf[..packet.len()].copy_from_slice(packet);
                packet.len()
            }
            UsbNetProtocol::CdcNcm => {
                let seq = inner.ncm_seq;
                inner.ncm_seq = inner.ncm_seq.wrapping_add(1);
                encode_cdc_ncm_tx_frame(seq, packet, tx_buf)
            }
        };
        if wire_len == 0 {
            return Err(Error::msg("Failed to encode USB Ethernet TX frame"));
        }

        // Send ZLP on Bulk-OUT when transfer is an exact multiple of wMaxPacketSize (per CDC/USB spec)
        let mps = inner.bulk_out_mps.max(64) as usize;
        let needs_zlp = wire_len.is_multiple_of(mps);
        let trbs_needed: u8 = if needs_zlp { 2 } else { 1 };
        if (inner.tx_trbs_in_flight as usize) + (trbs_needed as usize) > (RING_SIZE - 1) {
            return Err(Error::msg("USB Ethernet Bulk-OUT ring full"));
        }

        let bulk_out_virt = unsafe { inner.io_dma.virt.add(IO_OFF_BULK_OUT_RING) };
        let bulk_out_phys = inner.io_dma.phys + IO_OFF_BULK_OUT_RING as u64;
        let tx_phys = inner.io_dma.phys + tx_off as u64;

        g.enqueue_normal(
            bulk_out_virt,
            bulk_out_phys,
            &mut inner.bulk_out_ring,
            tx_phys,
            wire_len as u32,
            !needs_zlp,
            false,
        );
        if needs_zlp {
            g.enqueue_normal(
                bulk_out_virt,
                bulk_out_phys,
                &mut inner.bulk_out_ring,
                tx_phys,
                0,
                true,
                false,
            );
            inner.tx_zlp_mask |= 1u32 << slot_idx;
        } else {
            inner.tx_zlp_mask &= !(1u32 << slot_idx);
        }

        inner.tx_tail = ((slot_idx + 1) % NUM_TX_BUFS) as u8;
        inner.tx_in_flight += 1;
        inner.tx_trbs_in_flight += trbs_needed;

        g.ring_doorbell(inner.slot_id, inner.bulk_out_dci as u32);
        Ok(())
    }

    pub fn has_packets(&mut self) -> bool {
        let inner = &mut self.inner;
        if (inner.active_rx_buf as usize) < NUM_RX_BUFS {
            if inner.peek_has_next_active_frame() {
                return true;
            }
            let done_idx = inner.active_rx_buf as usize;
            inner.active_rx_buf = u8::MAX;
            if let Some(mut g) = self.ctrl.try_lock() {
                inner.requeue_rx_buf(&g, done_idx);
                inner.drain_rx_events(&mut g);
            }
            return inner.rx_ready_count > 0;
        }
        if inner.rx_ready_count > 0 {
            return true;
        }
        if let Some(mut g) = self.ctrl.try_lock() {
            inner.drain_rx_events(&mut g);
        }
        inner.rx_ready_count > 0
    }

    pub fn receive(&mut self, out: &mut [u8]) -> Option<usize> {
        if out.is_empty() {
            return None;
        }
        let inner = &mut self.inner;
        loop {
            if (inner.active_rx_buf as usize) < NUM_RX_BUFS {
                if let Some(len) = inner.extract_next_active_frame(out) {
                    return Some(len);
                }
                let done_idx = inner.active_rx_buf as usize;
                inner.active_rx_buf = u8::MAX;
                if let Some(g) = self.ctrl.try_lock() {
                    inner.requeue_rx_buf(&g, done_idx);
                }
            }

            if inner.rx_ready_count > 0 {
                let head = inner.rx_ready_head as usize;
                inner.active_rx_buf = inner.rx_ready_buf[head];
                inner.active_rx_len = inner.rx_ready_len[head];
                inner.active_rx_cursor = 0;
                inner.active_ncm_ndp = 0;
                inner.active_ncm_idx = 0;
                inner.rx_ready_head = ((head + 1) % NUM_RX_BUFS) as u8;
                inner.rx_ready_count -= 1;
                continue;
            }

            let mut g = self.ctrl.try_lock()?;
            inner.drain_rx_events(&mut g);
            if inner.rx_ready_count == 0 {
                return None;
            }
        }
    }

    pub fn link_up(&self) -> bool {
        let Some(g) = self.ctrl.try_lock() else {
            return true;
        };
        let portsc = g.read_portsc(self.inner.port_num);
        (portsc & crate::xhci::PORTSC_CCS) != 0 && (portsc & crate::xhci::PORTSC_PED) != 0
    }
}

impl UsbNetInner {
    fn reclaim_tx_events<H: Hal>(&mut self, g: &mut XhciController<H>) {
        let bulk_out_phys = self.io_dma.phys + IO_OFF_BULK_OUT_RING as u64;
        while let Some(ev) = g.pop_transfer_event(self.slot_id, self.bulk_out_dci) {
            if self.tx_in_flight > 0 {
                let head_idx = self.tx_head as usize;
                let trbs = if (self.tx_zlp_mask & (1u32 << head_idx)) != 0 {
                    2u8
                } else {
                    1u8
                };
                self.tx_zlp_mask &= !(1u32 << head_idx);
                self.tx_head = ((head_idx + 1) % NUM_TX_BUFS) as u8;
                self.tx_in_flight -= 1;
                self.tx_trbs_in_flight = self.tx_trbs_in_flight.saturating_sub(trbs);
            }
            let cc = ev.cc();
            if cc != CC_SUCCESS
                && cc != CC_SHORT_PACKET
                && matches!(
                    cc,
                    CC_STALL_ERROR | CC_USB_TX_ERROR | CC_DATA_BUFFER_ERROR | CC_BABBLE_DETECTED
                )
            {
                let _ = g.recover_halted_endpoint(
                    self.slot_id,
                    self.bulk_out_dci,
                    &mut self.bulk_out_ring,
                    bulk_out_phys,
                    cc == CC_STALL_ERROR,
                    &mut self.slot_dma,
                    &mut self.ep0_ring,
                );
            }
        }
    }

    fn drain_rx_events<H: Hal>(&mut self, g: &mut XhciController<H>) {
        let bulk_in_phys = self.slot_dma.phys + DEV_OFF_EP_RING as u64;
        while (self.rx_ready_count as usize) < NUM_RX_BUFS {
            let Some(ev) = g.pop_transfer_event(self.slot_id, self.bulk_in_dci) else {
                break;
            };
            let expected_idx = self.rx_head as usize;
            let expected_trb_phys = bulk_in_phys + (self.rx_trb_idx[expected_idx] as u64) * 16;

            let matched_idx = if ev.trb_ptr == 0 || ev.trb_ptr == expected_trb_phys {
                expected_idx
            } else if let Some(found) = (0..NUM_RX_BUFS)
                .find(|&i| bulk_in_phys + (self.rx_trb_idx[i] as u64) * 16 == ev.trb_ptr)
            {
                found
            } else {
                continue;
            };
            self.rx_head = ((matched_idx + 1) % NUM_RX_BUFS) as u8;

            let cc = ev.cc();
            if cc != CC_SUCCESS && cc != CC_SHORT_PACKET {
                if matches!(
                    cc,
                    CC_STALL_ERROR | CC_USB_TX_ERROR | CC_DATA_BUFFER_ERROR | CC_BABBLE_DETECTED
                ) {
                    let _ = g.recover_halted_endpoint(
                        self.slot_id,
                        self.bulk_in_dci,
                        &mut self.bulk_in_ring,
                        bulk_in_phys,
                        cc == CC_STALL_ERROR,
                        &mut self.slot_dma,
                        &mut self.ep0_ring,
                    );
                }
                self.requeue_rx_buf(g, matched_idx);
                continue;
            }

            let actual_len = BULK_BUFFER_SIZE.saturating_sub(ev.residual() as usize);
            if actual_len == 0 || actual_len > BULK_BUFFER_SIZE {
                self.requeue_rx_buf(g, matched_idx);
                continue;
            }
            fence(Ordering::Acquire);
            let transfer = unsafe {
                core::slice::from_raw_parts(
                    self.io_dma
                        .virt
                        .add(IO_OFF_RX_BUFS + matched_idx * BULK_BUFFER_SIZE),
                    actual_len,
                )
            };
            if !self.transfer_has_valid_frame(transfer) {
                self.requeue_rx_buf(g, matched_idx);
                continue;
            }

            let tail = self.rx_ready_tail as usize;
            self.rx_ready_buf[tail] = matched_idx as u8;
            self.rx_ready_len[tail] = actual_len as u16;
            self.rx_ready_tail = ((tail + 1) % NUM_RX_BUFS) as u8;
            self.rx_ready_count += 1;
        }
    }

    fn transfer_has_valid_frame(&self, transfer: &[u8]) -> bool {
        match self.protocol {
            UsbNetProtocol::Rtl8153 => decode_rtl8153_rx_frame(transfer).is_some(),
            UsbNetProtocol::CdcEcm => !transfer.is_empty(),
            UsbNetProtocol::CdcNcm => decode_cdc_ncm_rx_frame(transfer).is_some(),
        }
    }

    fn peek_has_next_active_frame(&self) -> bool {
        let buf_idx = self.active_rx_buf as usize;
        if buf_idx >= NUM_RX_BUFS {
            return false;
        }
        let raw_len = (self.active_rx_len as usize).min(BULK_BUFFER_SIZE);
        let transfer = unsafe {
            core::slice::from_raw_parts(
                self.io_dma
                    .virt
                    .add(IO_OFF_RX_BUFS + buf_idx * BULK_BUFFER_SIZE),
                raw_len,
            )
        };
        match self.protocol {
            UsbNetProtocol::Rtl8153 => {
                let mut cur = self.active_rx_cursor as usize;
                next_rtl8153_rx_frame(transfer, &mut cur).is_some()
            }
            UsbNetProtocol::CdcEcm => self.active_rx_cursor == 0 && raw_len > 0,
            UsbNetProtocol::CdcNcm => {
                let mut ndp = self.active_ncm_ndp as usize;
                let mut idx = self.active_ncm_idx as usize;
                next_cdc_ncm_rx_frame(transfer, &mut ndp, &mut idx).is_some()
            }
        }
    }

    fn extract_next_active_frame(&mut self, out: &mut [u8]) -> Option<usize> {
        let buf_idx = self.active_rx_buf as usize;
        if buf_idx >= NUM_RX_BUFS {
            return None;
        }
        let raw_len = (self.active_rx_len as usize).min(BULK_BUFFER_SIZE);
        let transfer = unsafe {
            core::slice::from_raw_parts(
                self.io_dma
                    .virt
                    .add(IO_OFF_RX_BUFS + buf_idx * BULK_BUFFER_SIZE),
                raw_len,
            )
        };

        let frame_slice = match self.protocol {
            UsbNetProtocol::Rtl8153 => {
                let mut cur = self.active_rx_cursor as usize;
                let res = next_rtl8153_rx_frame(transfer, &mut cur);
                self.active_rx_cursor = cur.min(raw_len) as u16;
                res
            }
            UsbNetProtocol::CdcEcm => {
                if self.active_rx_cursor == 0 {
                    self.active_rx_cursor = raw_len as u16;
                    Some(transfer)
                } else {
                    None
                }
            }
            UsbNetProtocol::CdcNcm => {
                let mut ndp = self.active_ncm_ndp as usize;
                let mut idx = self.active_ncm_idx as usize;
                let res = next_cdc_ncm_rx_frame(transfer, &mut ndp, &mut idx);
                self.active_ncm_ndp = ndp as u16;
                self.active_ncm_idx = idx as u16;
                res
            }
        }?;

        let copy_len = frame_slice.len().min(out.len());
        out[..copy_len].copy_from_slice(&frame_slice[..copy_len]);
        Some(copy_len)
    }

    fn requeue_rx_buf<H: Hal>(&mut self, g: &XhciController<H>, buf_idx: usize) {
        let bulk_in_virt = unsafe { self.slot_dma.virt.add(DEV_OFF_EP_RING) };
        let bulk_in_phys = self.slot_dma.phys + DEV_OFF_EP_RING as u64;
        let _ = RING_SIZE;
        let trb_phys = g.enqueue_normal(
            bulk_in_virt,
            bulk_in_phys,
            &mut self.bulk_in_ring,
            self.io_dma.phys + (IO_OFF_RX_BUFS + buf_idx * BULK_BUFFER_SIZE) as u64,
            BULK_BUFFER_SIZE as u32,
            true,
            true,
        );
        self.rx_trb_idx[buf_idx] = ((trb_phys - bulk_in_phys) / 16) as u16;
        g.ring_doorbell(self.slot_id, self.bulk_in_dci as u32);
    }
}

impl<H: Hal> Drop for UsbEthernetDevice<H> {
    fn drop(&mut self) {
        if let Some(mut g) = self.ctrl.try_lock() {
            let disabled = g.disable_slot(self.inner.slot_id).is_ok() || g.is_halted();
            g.release_port(self.inner.port_num);
            if disabled {
                let io_dma = unsafe { ManuallyDrop::take(&mut self.inner.io_dma) };
                let slot_dma = unsafe { ManuallyDrop::take(&mut self.inner.slot_dma) };
                g.hal().free_dma(io_dma);
                g.hal().free_dma(slot_dma);
                let _ = spin_wait_us::<H>;
            }
        }
    }
}
