use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::ptr;
use core::sync::atomic::{Ordering, fence};

use crate::error::{Error, Result};
use crate::hal::{DmaBuffer, Hal, PAGE_SIZE};

const RING_SIZE: usize = 64;
const BULK_BUF_COUNT: usize = 32;
pub const BULK_BUFFER_SIZE: usize = 2048;

const TRB_NORMAL: u32 = 1;
const TRB_SETUP_STAGE: u32 = 2;
const TRB_DATA_STAGE: u32 = 3;
const TRB_STATUS_STAGE: u32 = 4;
const TRB_LINK: u32 = 6;
const TRB_ENABLE_SLOT: u32 = 9;
const TRB_ADDRESS_DEVICE: u32 = 11;
const TRB_CONFIGURE_ENDPOINT: u32 = 12;
const TRB_TRANSFER_EVENT: u32 = 32;
const TRB_CMD_COMPLETION: u32 = 33;

const OFF_DCBAA: usize = 0x0000;
const OFF_ERST: usize = 0x0800;
const OFF_CTRL_BUF: usize = 0x0900;
const OFF_CMD_RING: usize = 0x1000;
const OFF_EVENT_RING: usize = 0x1400;
const OFF_EP0_RING: usize = 0x1800;
const OFF_BULK_IN_RING: usize = 0x1C00;
const OFF_BULK_OUT_RING: usize = 0x2000;
const OFF_OUT_CTX: usize = 0x2400;
const OFF_IN_CTX: usize = 0x2C00;

/// Supported USB Ethernet dongle protocol modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsbNetProtocol {
    /// USB CDC-ECM (Communications Device Class - Ethernet Control Model, SubClass 0x06):
    /// Raw Ethernet II frames over Bulk-IN and Bulk-OUT endpoints.
    CdcEcm,
    /// Realtek RTL8152 / RTL8153 / RTL8156 (`0bda:8152`, `0bda:8153`, `0bda:8156`) native vendor mode:
    /// Ethernet frames prefixed by an 8-byte `RtlUsbTxDesc` / `RtlUsbRxDesc` header.
    Rtl8153,
    /// USB CDC-NCM (Network Control Model, SubClass 0x0D):
    /// Ethernet frames encapsulated in 16-bit NTH16 + NDP16 USB transfer blocks.
    CdcNcm,
}

/// Prepends the 8-byte Realtek RTL8152/8153/8156 USB TX descriptor (`opts1 = TX_FS | TX_LS | len`, `opts2 = 0`)
/// to an Ethernet frame.
pub fn encode_rtl8153_tx_frame(frame: &[u8], out: &mut [u8]) -> usize {
    if out.len() < frame.len() + 8 {
        return 0;
    }
    let opts1: u32 = (1 << 31) | (1 << 30) | (frame.len() as u32 & 0xFFFF);
    out[0..4].copy_from_slice(&opts1.to_le_bytes());
    out[4..8].fill(0);
    out[8..8 + frame.len()].copy_from_slice(frame);
    frame.len() + 8
}

/// Decodes a Realtek RTL8152/8153/8156 USB RX transfer (8-byte `RtlUsbRxDesc` header + Ethernet frame + 4-byte FCS),
/// returning the inner Ethernet frame slice without the 8-byte header or 4-byte trailing CRC.
pub fn decode_rtl8153_rx_frame(transfer: &[u8]) -> Option<&[u8]> {
    if transfer.len() < 8 + 14 {
        return None;
    }
    let opts1 = u32::from_le_bytes(transfer[0..4].try_into().ok()?);
    let raw_len = (opts1 & 0x7FFF) as usize;
    let payload_avail = transfer.len() - 8;
    let frame_with_crc = raw_len.min(payload_avail);
    let frame_len = if frame_with_crc >= 18 {
        frame_with_crc - 4
    } else {
        frame_with_crc
    };
    if frame_len == 0 || 8 + frame_len > transfer.len() {
        return None;
    }
    Some(&transfer[8..8 + frame_len])
}

/// Wraps an Ethernet frame in a minimal 16-bit USB CDC-NCM Transfer Block (`NTH16` + `NDP16` + frame).
pub fn encode_cdc_ncm_tx_frame(seq: u16, frame: &[u8], out: &mut [u8]) -> usize {
    // 12-byte NTH16 + 16-byte NDP16 (with 1 datagram pointer + terminating 0,0 pointer) = 28 bytes header
    if out.len() < 28 + frame.len() {
        return 0;
    }
    let block_len = (28 + frame.len()) as u16;
    // NTH16
    out[0..4].copy_from_slice(b"NCMH");
    out[4..6].copy_from_slice(&12u16.to_le_bytes()); // wHeaderLength = 12
    out[6..8].copy_from_slice(&seq.to_le_bytes()); // wSequence
    out[8..10].copy_from_slice(&block_len.to_le_bytes()); // wBlockLength
    out[10..12].copy_from_slice(&12u16.to_le_bytes()); // wNdpIndex = 12

    // NDP16 at offset 12
    out[12..16].copy_from_slice(b"NCM0");
    out[16..18].copy_from_slice(&16u16.to_le_bytes()); // wLength = 16
    out[18..20].copy_from_slice(&0u16.to_le_bytes()); // wNextNdpIndex = 0
    out[20..22].copy_from_slice(&28u16.to_le_bytes()); // wDatagramIndex[0] = 28
    out[22..24].copy_from_slice(&(frame.len() as u16).to_le_bytes()); // wDatagramLength[0]
    out[24..28].fill(0); // Terminating entry (0, 0)

    out[28..28 + frame.len()].copy_from_slice(frame);
    28 + frame.len()
}

/// Extracts the first Ethernet frame from a 16-bit USB CDC-NCM Transfer Block (`NTH16` + `NDP16`).
pub fn decode_cdc_ncm_rx_frame(ntb: &[u8]) -> Option<&[u8]> {
    if ntb.len() < 28 || &ntb[0..4] != b"NCMH" {
        return None;
    }
    let ndp_idx = u16::from_le_bytes(ntb[10..12].try_into().ok()?) as usize;
    if ndp_idx + 16 > ntb.len() || &ntb[ndp_idx..ndp_idx + 3] != b"NCM" {
        return None;
    }
    let dg_idx = u16::from_le_bytes(ntb[ndp_idx + 8..ndp_idx + 10].try_into().ok()?) as usize;
    let dg_len = u16::from_le_bytes(ntb[ndp_idx + 10..ndp_idx + 12].try_into().ok()?) as usize;
    if dg_idx == 0 || dg_len == 0 || dg_idx + dg_len > ntb.len() {
        return None;
    }
    Some(&ntb[dg_idx..dg_idx + dg_len])
}

pub struct UsbEthernetDevice<H: Hal> {
    pub mmio_base: u64,
    pub protocol: UsbNetProtocol,
    pub hal: H,
    op_base: u64,
    rt_base: u64,
    db_base: u64,
    ctrl_dma: DmaBuffer,
    rx_bufs: DmaBuffer,
    tx_bufs: DmaBuffer,
    cmd_enqueue: usize,
    cmd_cycle: bool,
    er_dequeue: usize,
    er_cycle: bool,
    ep0_enqueue: usize,
    ep0_cycle: bool,
    bulk_in_enqueue: usize,
    bulk_in_cycle: bool,
    bulk_out_enqueue: usize,
    bulk_out_cycle: bool,
    bulk_out_in_flight: usize,
    active_slot: u8,
    bulk_in_dci: u8,
    bulk_out_dci: u8,
    mac: [u8; 6],
    ncm_seq: u16,
    rx_ready: VecDeque<Vec<u8>>,
}

impl<H: Hal> UsbEthernetDevice<H> {
    /// Initializes an xHCI controller at `mmio_base` and enumerates a connected USB Ethernet dongle
    /// (supporting USB CDC-ECM, CDC-NCM, and Realtek RTL8152/RTL8153/RTL8156):
    /// - Performs `USBLEGSUP` BIOS-to-OS handoff.
    /// - Resets and starts the xHCI controller with `DCBAA`, Command Ring, Event Ring, EP0 Control Ring,
    ///   Bulk-IN Ring, and Bulk-OUT Ring.
    /// - Queries USB Device and Configuration descriptors on connected ports to identify the protocol
    ///   (`Rtl8153`, `CdcEcm`, or `CdcNcm`), Bulk-IN / Bulk-OUT endpoints, and MAC address.
    /// - Queues Bulk-IN TRBs for non-blocking Ethernet packet reception.
    pub fn new(mmio_base: u64, fallback_mac: Option<[u8; 6]>, hal: H) -> Result<Self> {
        if mmio_base == 0 {
            return Err(Error::msg("USB Ethernet xHCI MMIO base is 0"));
        }
        hal.map_mmio(mmio_base, 0x10000);
        let cap_hciver = hal.mmio_read32(mmio_base);
        if cap_hciver == 0xFFFF_FFFF || cap_hciver == 0 {
            return Err(Error::msg("xHCI controller not present"));
        }
        let cap_length = (cap_hciver & 0xFF) as u64;
        let hcsparams1 = hal.mmio_read32(mmio_base + 0x04);
        let hccparams1 = hal.mmio_read32(mmio_base + 0x10);
        let dboff = (hal.mmio_read32(mmio_base + 0x14) & !0x3) as u64;
        let rtsoff = (hal.mmio_read32(mmio_base + 0x18) & !0x1F) as u64;

        let max_slots = (hcsparams1 & 0xFF).clamp(1, 32);
        let max_ports = ((hcsparams1 >> 24) & 0xFF) as usize;
        let ctx_size = if (hccparams1 & (1 << 2)) != 0 { 64 } else { 32 };

        let op_base = mmio_base + cap_length;
        let rt_base = mmio_base + rtsoff;
        let db_base = mmio_base + dboff;

        // 1. BIOS-to-OS handoff via USBLEGSUP extended capability (id == 1)
        let mut ext_off = ((hccparams1 >> 16) as u64) * 4;
        for _ in 0..64 {
            if ext_off == 0 {
                break;
            }
            let cap = hal.mmio_read32(mmio_base + ext_off);
            if cap == 0 || cap == 0xFFFF_FFFF {
                break;
            }
            if (cap & 0xFF) == 1 {
                hal.mmio_write32(mmio_base + ext_off, cap | (1 << 24));
                for _ in 0..200 {
                    if (hal.mmio_read32(mmio_base + ext_off) & (1 << 16)) == 0 {
                        break;
                    }
                }
                hal.mmio_write32(mmio_base + ext_off + 4, 0);
                break;
            }
            let next_dw = ((cap >> 8) & 0xFF) as u64;
            if next_dw == 0 {
                break;
            }
            ext_off += next_dw * 4;
        }

        // 2. Stop and reset controller (USBCMD.HCRST)
        let cmd = hal.mmio_read32(op_base);
        if (cmd & 1) != 0 {
            hal.mmio_write32(op_base, cmd & !1);
            for _ in 0..500 {
                if (hal.mmio_read32(op_base + 0x04) & 1) != 0 {
                    break;
                }
            }
        }
        hal.mmio_write32(op_base, 1 << 1);
        let mut reset_done = false;
        for _ in 0..2000 {
            let cmd = hal.mmio_read32(op_base);
            let sts = hal.mmio_read32(op_base + 0x04);
            if (cmd & (1 << 1)) == 0 && (sts & (1 << 11)) == 0 {
                reset_done = true;
                break;
            }
        }
        if !reset_done {
            return Err(Error::msg("xHCI USB Ethernet HCRST timeout"));
        }

        // 3. Allocate DMA structures
        let ctrl_dma = hal
            .alloc_dma(4)
            .ok_or_else(|| Error::msg("Out of DMA memory for USB Ethernet control rings"))?;
        let rx_buf_pages = (BULK_BUF_COUNT * BULK_BUFFER_SIZE).div_ceil(PAGE_SIZE);
        let tx_buf_pages = (BULK_BUF_COUNT * BULK_BUFFER_SIZE).div_ceil(PAGE_SIZE);
        let Some(rx_bufs) = hal.alloc_dma(rx_buf_pages) else {
            hal.free_dma(ctrl_dma);
            return Err(Error::msg("Out of DMA memory for USB Ethernet RX buffers"));
        };
        let Some(tx_bufs) = hal.alloc_dma(tx_buf_pages) else {
            hal.free_dma(ctrl_dma);
            hal.free_dma(rx_bufs);
            return Err(Error::msg("Out of DMA memory for USB Ethernet TX buffers"));
        };

        unsafe {
            ptr::write_bytes(ctrl_dma.virt, 0, ctrl_dma.size);
        }

        for ring_off in [
            OFF_CMD_RING,
            OFF_EP0_RING,
            OFF_BULK_IN_RING,
            OFF_BULK_OUT_RING,
        ] {
            Self::write_trb_raw(
                unsafe { ctrl_dma.virt.add(ring_off + (RING_SIZE - 1) * 16) },
                ctrl_dma.phys + ring_off as u64,
                0,
                (TRB_LINK << 10) | (1 << 1) | 1,
            );
        }

        unsafe {
            let erst_ptr = ctrl_dma.virt.add(OFF_ERST) as *mut u32;
            let er_phys = ctrl_dma.phys + OFF_EVENT_RING as u64;
            ptr::write_volatile(erst_ptr, er_phys as u32);
            ptr::write_volatile(erst_ptr.add(1), (er_phys >> 32) as u32);
            ptr::write_volatile(erst_ptr.add(2), RING_SIZE as u32);
            ptr::write_volatile(erst_ptr.add(3), 0);
        }

        let dcbaa_phys = ctrl_dma.phys + OFF_DCBAA as u64;
        let cmd_ring_phys = ctrl_dma.phys + OFF_CMD_RING as u64;
        let er_phys = ctrl_dma.phys + OFF_EVENT_RING as u64;
        let erst_phys = ctrl_dma.phys + OFF_ERST as u64;
        let ir0 = rt_base + 0x20;

        hal.mmio_write32(op_base + 0x38, max_slots);
        hal.mmio_write32(op_base + 0x30, dcbaa_phys as u32);
        hal.mmio_write32(op_base + 0x34, (dcbaa_phys >> 32) as u32);
        hal.mmio_write32(op_base + 0x18, (cmd_ring_phys as u32) | 1);
        hal.mmio_write32(op_base + 0x1C, (cmd_ring_phys >> 32) as u32);

        hal.mmio_write32(ir0 + 0x04, 0);
        hal.mmio_write32(ir0 + 0x08, 1);
        hal.mmio_write32(ir0 + 0x18, (er_phys as u32) | (1 << 3));
        hal.mmio_write32(ir0 + 0x1C, (er_phys >> 32) as u32);
        hal.mmio_write32(ir0 + 0x10, erst_phys as u32);
        hal.mmio_write32(ir0 + 0x14, (erst_phys >> 32) as u32);
        hal.mmio_write32(ir0, 0x02);

        hal.mmio_write32(op_base, 1 | (1 << 2));
        for _ in 0..500 {
            if (hal.mmio_read32(op_base + 0x04) & 1) == 0 {
                break;
            }
        }

        let default_mac = fallback_mac.unwrap_or([0x02, 0x50, 0x49, 0x58, 0x49, 0x45]);
        let mut dev = Self {
            mmio_base,
            protocol: UsbNetProtocol::CdcEcm,
            hal,
            op_base,
            rt_base,
            db_base,
            ctrl_dma,
            rx_bufs,
            tx_bufs,
            cmd_enqueue: 0,
            cmd_cycle: true,
            er_dequeue: 0,
            er_cycle: true,
            ep0_enqueue: 0,
            ep0_cycle: true,
            bulk_in_enqueue: 0,
            bulk_in_cycle: true,
            bulk_out_enqueue: 0,
            bulk_out_cycle: true,
            bulk_out_in_flight: 0,
            active_slot: 0,
            bulk_in_dci: 3,  // Default EP1 IN (DCI = 3)
            bulk_out_dci: 4, // Default EP2 OUT (DCI = 4)
            mac: default_mac,
            ncm_seq: 0,
            rx_ready: VecDeque::new(),
        };

        let mut initialized = false;
        for port_idx in 0..max_ports {
            let portsc_addr = op_base + 0x400 + (port_idx as u64) * 0x10;
            let mut portsc = dev.hal.mmio_read32(portsc_addr);
            if (portsc & 1) == 0 {
                continue;
            }
            if (portsc & (1 << 1)) == 0 {
                dev.hal
                    .mmio_write32(portsc_addr, (portsc & 0x0E01_C3E0) | (1 << 4));
                for _ in 0..200 {
                    if (dev.hal.mmio_read32(portsc_addr) & (1 << 4)) == 0 {
                        break;
                    }
                }
                portsc = dev.hal.mmio_read32(portsc_addr);
            }
            let speed = (((portsc >> 10) & 0xF) as u8).max(1);
            if dev.init_port_nic(port_idx + 1, speed, ctx_size).is_ok() {
                initialized = true;
                break;
            }
        }

        if !initialized {
            return Err(Error::msg("No USB Ethernet dongle found on xHCI ports"));
        }

        Ok(dev)
    }

    fn init_port_nic(&mut self, port_num: usize, speed: u8, ctx_size: usize) -> Result<()> {
        let dma_virt = self.ctrl_dma.virt;
        let dma_phys = self.ctrl_dma.phys;

        // 1. Enable Slot
        self.enqueue_cmd(0, 0, TRB_ENABLE_SLOT << 10);
        let slot_id = match self.wait_cmd_completion() {
            Some((cc, slot)) if (cc == 1 || cc == 0) && slot != 0 => slot,
            _ => 1,
        };
        self.active_slot = slot_id;

        let out_ctx_phys = dma_phys + OFF_OUT_CTX as u64;
        unsafe {
            let dcbaa_entry = dma_virt.add(OFF_DCBAA + (slot_id as usize) * 8) as *mut u64;
            ptr::write_volatile(dcbaa_entry, out_ctx_phys);
        }

        // 2. Address Device (Slot + EP0)
        let in_ctx_virt = unsafe { dma_virt.add(OFF_IN_CTX) };
        let in_ctx_phys = dma_phys + OFF_IN_CTX as u64;
        unsafe {
            ptr::write_bytes(in_ctx_virt, 0, 0x800);
            ptr::write_volatile(in_ctx_virt.add(4) as *mut u32, (1 << 0) | (1 << 1));

            let slot_ctx = in_ctx_virt.add(ctx_size) as *mut u32;
            ptr::write_volatile(slot_ctx, (1 << 27) | ((speed as u32) << 20));
            ptr::write_volatile(slot_ctx.add(1), (port_num as u32) << 16);

            let ep0_ctx = in_ctx_virt.add(2 * ctx_size) as *mut u32;
            let ep0_ring_phys = dma_phys + OFF_EP0_RING as u64;
            let ep0_max_pkt = if speed >= 4 { 512 } else { 64 };
            ptr::write_volatile(ep0_ctx.add(1), (4 << 3) | (3 << 1) | (ep0_max_pkt << 16));
            ptr::write_volatile(ep0_ctx.add(2), (ep0_ring_phys as u32) | 1);
            ptr::write_volatile(ep0_ctx.add(3), (ep0_ring_phys >> 32) as u32);
            ptr::write_volatile(ep0_ctx.add(4), 8);
        }

        self.enqueue_cmd(
            in_ctx_phys,
            0,
            (TRB_ADDRESS_DEVICE << 10) | ((slot_id as u32) << 24),
        );
        let _ = self.wait_cmd_completion();

        // 3. Query Device Descriptor (18 bytes) via EP0 Control Transfer
        let mut dev_desc = [0u8; 18];
        if self.control_in([0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 18, 0], &mut dev_desc) {
            let vid = u16::from_le_bytes([dev_desc[8], dev_desc[9]]);
            let pid = u16::from_le_bytes([dev_desc[10], dev_desc[11]]);
            let b_device_class = dev_desc[4];
            if vid == 0x0BDA && matches!(pid, 0x8152 | 0x8153 | 0x8156) {
                self.protocol = UsbNetProtocol::Rtl8153;
            } else if b_device_class == 0x03 {
                // Pure HID class device (keyboard/mouse), not a USB NIC
                return Err(Error::msg("USB device is HID, not Ethernet"));
            }
        }

        // 4. Query Configuration Descriptor (up to 128 bytes) to discover Bulk IN / OUT endpoints and protocol
        let mut cfg_desc = [0u8; 128];
        if self.control_in([0x80, 0x06, 0x00, 0x02, 0x00, 0x00, 128, 0], &mut cfg_desc) {
            let total_len = (u16::from_le_bytes([cfg_desc[2], cfg_desc[3]]) as usize).min(128);
            let mut off = 0usize;
            let mut found_nic_iface = self.protocol == UsbNetProtocol::Rtl8153;
            while off + 2 <= total_len {
                let b_len = cfg_desc[off] as usize;
                let b_type = cfg_desc[off + 1];
                if b_len < 2 || off + b_len > total_len {
                    break;
                }
                if b_type == 4 && b_len >= 9 {
                    // Interface Descriptor
                    let class = cfg_desc[off + 5];
                    let subclass = cfg_desc[off + 6];
                    if class == 0x02 && subclass == 0x06 {
                        self.protocol = UsbNetProtocol::CdcEcm;
                        found_nic_iface = true;
                    } else if class == 0x02 && subclass == 0x0D {
                        self.protocol = UsbNetProtocol::CdcNcm;
                        found_nic_iface = true;
                    } else if class == 0x0A || class == 0xFF {
                        found_nic_iface = true;
                    } else if class == 0x03 {
                        return Err(Error::msg("USB interface is HID"));
                    }
                } else if b_type == 5 && b_len >= 7 {
                    // Endpoint Descriptor
                    let ep_addr = cfg_desc[off + 2];
                    let attr = cfg_desc[off + 3] & 0x03;
                    if attr == 0x02 {
                        // Bulk endpoint!
                        let ep_num = ep_addr & 0x0F;
                        let is_in = (ep_addr & 0x80) != 0;
                        let dci = ep_num * 2 + if is_in { 1 } else { 0 };
                        if is_in {
                            self.bulk_in_dci = dci.clamp(2, 31);
                        } else {
                            self.bulk_out_dci = dci.clamp(2, 31);
                        }
                    }
                }
                off += b_len;
            }
            if !found_nic_iface {
                return Err(Error::msg("No USB Ethernet interface found"));
            }
        }

        // If Realtek RTL8152/8153, try reading 6-byte MAC from PLA IDR (`0xC000`, `wIndex = 0x0100`)
        if self.protocol == UsbNetProtocol::Rtl8153 {
            let mut pla_mac = [0u8; 8];
            if self.control_in([0xC0, 0x05, 0x00, 0xC0, 0x00, 0x01, 8, 0], &mut pla_mac) {
                let candidate: [u8; 6] = pla_mac[0..6].try_into().unwrap();
                if candidate != [0; 6] && candidate != [0xFF; 6] && (candidate[0] & 1) == 0 {
                    self.mac = candidate;
                }
            }
        }

        // 5. Configure Bulk-IN and Bulk-OUT Endpoints in xHCI
        let max_dci = self.bulk_in_dci.max(self.bulk_out_dci) as u32;
        let bulk_max_pkt = if speed >= 4 { 1024u32 } else { 512u32 };
        unsafe {
            ptr::write_bytes(in_ctx_virt, 0, 0x800);
            let add_flags = (1u32 << 0) | (1u32 << self.bulk_in_dci) | (1u32 << self.bulk_out_dci);
            ptr::write_volatile(in_ctx_virt.add(4) as *mut u32, add_flags);

            let slot_ctx = in_ctx_virt.add(ctx_size) as *mut u32;
            ptr::write_volatile(slot_ctx, (max_dci << 27) | ((speed as u32) << 20));
            ptr::write_volatile(slot_ctx.add(1), (port_num as u32) << 16);

            // Bulk-IN Endpoint Context (EP Type = 6 = Bulk IN)
            let in_ep_ctx = in_ctx_virt.add((self.bulk_in_dci as usize + 1) * ctx_size) as *mut u32;
            let bulk_in_ring_phys = dma_phys + OFF_BULK_IN_RING as u64;
            ptr::write_volatile(in_ep_ctx.add(1), (6 << 3) | (3 << 1) | (bulk_max_pkt << 16));
            ptr::write_volatile(in_ep_ctx.add(2), (bulk_in_ring_phys as u32) | 1);
            ptr::write_volatile(in_ep_ctx.add(3), (bulk_in_ring_phys >> 32) as u32);
            ptr::write_volatile(in_ep_ctx.add(4), BULK_BUFFER_SIZE as u32);

            // Bulk-OUT Endpoint Context (EP Type = 2 = Bulk OUT)
            let out_ep_ctx =
                in_ctx_virt.add((self.bulk_out_dci as usize + 1) * ctx_size) as *mut u32;
            let bulk_out_ring_phys = dma_phys + OFF_BULK_OUT_RING as u64;
            ptr::write_volatile(
                out_ep_ctx.add(1),
                (2 << 3) | (3 << 1) | (bulk_max_pkt << 16),
            );
            ptr::write_volatile(out_ep_ctx.add(2), (bulk_out_ring_phys as u32) | 1);
            ptr::write_volatile(out_ep_ctx.add(3), (bulk_out_ring_phys >> 32) as u32);
            ptr::write_volatile(out_ep_ctx.add(4), BULK_BUFFER_SIZE as u32);
        }

        self.enqueue_cmd(
            in_ctx_phys,
            0,
            (TRB_CONFIGURE_ENDPOINT << 10) | ((slot_id as u32) << 24),
        );
        let _ = self.wait_cmd_completion();

        // 6. Queue initial Bulk-IN receive TRBs
        for _ in 0..16 {
            self.queue_bulk_in_trb();
        }
        self.ring_doorbell(self.bulk_in_dci);

        Ok(())
    }

    fn control_in(&mut self, setup: [u8; 8], out: &mut [u8]) -> bool {
        let dma_virt = self.ctrl_dma.virt;
        let dma_phys = self.ctrl_dma.phys;
        let data_phys = dma_phys + OFF_CTRL_BUF as u64;
        unsafe {
            ptr::write_bytes(dma_virt.add(OFF_CTRL_BUF), 0, out.len().min(256));
        }

        // 1. Setup Stage TRB (TRT = 3 IN Data Stage, Immediate Data = bit 6, length = 8)
        let setup_param = u64::from_le_bytes(setup);
        self.enqueue_ep0(
            setup_param,
            8,
            (TRB_SETUP_STAGE << 10) | (3 << 16) | (1 << 6),
        );

        // 2. Data Stage TRB (DIR = 1 IN = bit 16)
        self.enqueue_ep0(
            data_phys,
            out.len() as u32,
            (TRB_DATA_STAGE << 10) | (1 << 16),
        );

        // 3. Status Stage TRB (DIR = 0 OUT, IOC = bit 5)
        self.enqueue_ep0(0, 0, (TRB_STATUS_STAGE << 10) | (1 << 5));

        self.ring_doorbell(1); // EP0 doorbell = 1

        for _ in 0..200 {
            if let Some((trb_type, _param, status, _control)) = self.pop_event_trb()
                && trb_type == TRB_TRANSFER_EVENT
            {
                let cc = ((status >> 24) & 0xFF) as u8;
                if cc == 1 || cc == 13 {
                    unsafe {
                        ptr::copy_nonoverlapping(
                            dma_virt.add(OFF_CTRL_BUF) as *const u8,
                            out.as_mut_ptr(),
                            out.len(),
                        );
                    }
                    return true;
                }
                return false;
            }
        }
        false
    }

    fn write_trb_raw(addr: *mut u8, param: u64, status: u32, control: u32) {
        unsafe {
            let p = addr as *mut u32;
            ptr::write_volatile(p, param as u32);
            ptr::write_volatile(p.add(1), (param >> 32) as u32);
            ptr::write_volatile(p.add(2), status);
            ptr::write_volatile(p.add(3), control);
        }
    }

    fn enqueue_ep0(&mut self, param: u64, status: u32, control: u32) {
        let idx = self.ep0_enqueue;
        let cycle = self.ep0_cycle as u32;
        Self::write_trb_raw(
            unsafe { self.ctrl_dma.virt.add(OFF_EP0_RING + idx * 16) },
            param,
            status,
            (control & !1) | cycle,
        );
        self.ep0_enqueue += 1;
        if self.ep0_enqueue >= RING_SIZE - 1 {
            let link_addr = unsafe { self.ctrl_dma.virt.add(OFF_EP0_RING + (RING_SIZE - 1) * 16) };
            Self::write_trb_raw(
                link_addr,
                self.ctrl_dma.phys + OFF_EP0_RING as u64,
                0,
                (TRB_LINK << 10) | (1 << 1) | cycle,
            );
            self.ep0_enqueue = 0;
            self.ep0_cycle = !self.ep0_cycle;
        }
    }

    fn enqueue_cmd(&mut self, param: u64, status: u32, control: u32) {
        let idx = self.cmd_enqueue;
        let cycle = self.cmd_cycle as u32;
        Self::write_trb_raw(
            unsafe { self.ctrl_dma.virt.add(OFF_CMD_RING + idx * 16) },
            param,
            status,
            (control & !1) | cycle,
        );
        self.cmd_enqueue += 1;
        if self.cmd_enqueue >= RING_SIZE - 1 {
            let link_addr = unsafe { self.ctrl_dma.virt.add(OFF_CMD_RING + (RING_SIZE - 1) * 16) };
            Self::write_trb_raw(
                link_addr,
                self.ctrl_dma.phys + OFF_CMD_RING as u64,
                0,
                (TRB_LINK << 10) | (1 << 1) | cycle,
            );
            self.cmd_enqueue = 0;
            self.cmd_cycle = !self.cmd_cycle;
        }
        self.hal.mmio_write32(self.db_base, 0);
    }

    fn wait_cmd_completion(&mut self) -> Option<(u8, u8)> {
        for _ in 0..200 {
            if let Some((trb_type, _param, status, control)) = self.pop_event_trb()
                && trb_type == TRB_CMD_COMPLETION
            {
                let cc = ((status >> 24) & 0xFF) as u8;
                let slot = ((control >> 24) & 0xFF) as u8;
                return Some((cc, slot));
            }
        }
        None
    }

    fn queue_bulk_in_trb(&mut self) {
        let idx = self.bulk_in_enqueue;
        let buf_idx = idx % BULK_BUF_COUNT;
        let cycle = self.bulk_in_cycle as u32;
        let buf_phys = self.rx_bufs.phys + (buf_idx * BULK_BUFFER_SIZE) as u64;
        Self::write_trb_raw(
            unsafe { self.ctrl_dma.virt.add(OFF_BULK_IN_RING + idx * 16) },
            buf_phys,
            BULK_BUFFER_SIZE as u32,
            (TRB_NORMAL << 10) | (1 << 5) | (1 << 2) | cycle, // IOC = 1, ISP = 1
        );
        self.bulk_in_enqueue += 1;
        if self.bulk_in_enqueue >= RING_SIZE - 1 {
            let link_addr = unsafe {
                self.ctrl_dma
                    .virt
                    .add(OFF_BULK_IN_RING + (RING_SIZE - 1) * 16)
            };
            Self::write_trb_raw(
                link_addr,
                self.ctrl_dma.phys + OFF_BULK_IN_RING as u64,
                0,
                (TRB_LINK << 10) | (1 << 1) | cycle,
            );
            self.bulk_in_enqueue = 0;
            self.bulk_in_cycle = !self.bulk_in_cycle;
        }
    }

    fn ring_doorbell(&self, dci: u8) {
        let slot = self.active_slot.max(1) as u64;
        self.hal.mmio_write32(self.db_base + slot * 4, dci as u32);
    }

    fn pop_event_trb(&mut self) -> Option<(u32, u64, u32, u32)> {
        let er_virt = unsafe {
            self.ctrl_dma
                .virt
                .add(OFF_EVENT_RING + self.er_dequeue * 16)
        };
        let (param_lo, param_hi, status, control) = unsafe {
            let p = er_virt as *const u32;
            (
                ptr::read_volatile(p),
                ptr::read_volatile(p.add(1)),
                ptr::read_volatile(p.add(2)),
                ptr::read_volatile(p.add(3)),
            )
        };
        if ((control & 1) != 0) != self.er_cycle {
            return None;
        }

        self.er_dequeue += 1;
        if self.er_dequeue >= RING_SIZE {
            self.er_dequeue = 0;
            self.er_cycle = !self.er_cycle;
        }

        let erdp = self.ctrl_dma.phys + (OFF_EVENT_RING + self.er_dequeue * 16) as u64;
        let ir0 = self.rt_base + 0x20;
        self.hal.mmio_write32(ir0 + 0x18, (erdp as u32) | (1 << 3));
        self.hal.mmio_write32(ir0 + 0x1C, (erdp >> 32) as u32);

        let trb_type = (control >> 10) & 0x3F;
        let param = ((param_hi as u64) << 32) | (param_lo as u64);
        Some((trb_type, param, status, control))
    }

    fn poll_events(&mut self) {
        let bulk_in_ring_phys = self.ctrl_dma.phys + OFF_BULK_IN_RING as u64;
        let bulk_out_ring_phys = self.ctrl_dma.phys + OFF_BULK_OUT_RING as u64;

        for _ in 0..RING_SIZE {
            let Some((trb_type, param, status, control)) = self.pop_event_trb() else {
                break;
            };
            if trb_type != TRB_TRANSFER_EVENT {
                continue;
            }
            let ep_dci = ((control >> 16) & 0x1F) as u8;
            let residual = (status & 0x00FF_FFFF) as usize;

            if ep_dci == self.bulk_out_dci
                || (param >= bulk_out_ring_phys
                    && param < bulk_out_ring_phys + (RING_SIZE * 16) as u64)
            {
                self.bulk_out_in_flight = self.bulk_out_in_flight.saturating_sub(1);
                continue;
            }

            if ep_dci == self.bulk_in_dci
                || (param >= bulk_in_ring_phys
                    && param < bulk_in_ring_phys + (RING_SIZE * 16) as u64)
            {
                let (buf_phys, trb_len) = if param >= bulk_in_ring_phys
                    && param < bulk_in_ring_phys + (RING_SIZE * 16) as u64
                {
                    let trb_idx = ((param - bulk_in_ring_phys) / 16) as usize;
                    let trb_virt =
                        unsafe { self.ctrl_dma.virt.add(OFF_BULK_IN_RING + trb_idx * 16) };
                    let p = unsafe { ptr::read_volatile(trb_virt as *const u64) };
                    let st = unsafe { ptr::read_volatile((trb_virt as *const u32).add(2)) };
                    (p, (st & 0x1FFFF) as usize)
                } else {
                    (param, BULK_BUFFER_SIZE)
                };

                let transferred = trb_len.saturating_sub(residual).min(BULK_BUFFER_SIZE);
                if transferred > 0
                    && buf_phys >= self.rx_bufs.phys
                    && buf_phys + transferred as u64 <= self.rx_bufs.phys + self.rx_bufs.size as u64
                {
                    let off = (buf_phys - self.rx_bufs.phys) as usize;
                    let raw_slice = unsafe {
                        core::slice::from_raw_parts(self.rx_bufs.virt.add(off), transferred)
                    };
                    let decoded = match self.protocol {
                        UsbNetProtocol::CdcEcm => Some(raw_slice),
                        UsbNetProtocol::Rtl8153 => decode_rtl8153_rx_frame(raw_slice),
                        UsbNetProtocol::CdcNcm => decode_cdc_ncm_rx_frame(raw_slice),
                    };
                    if let Some(frame) = decoded
                        && !frame.is_empty()
                    {
                        self.rx_ready.push_back(frame.to_vec());
                    }
                }

                self.queue_bulk_in_trb();
                self.ring_doorbell(self.bulk_in_dci);
            }
        }
    }

    pub fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    pub fn protocol(&self) -> UsbNetProtocol {
        self.protocol
    }

    pub fn can_transmit(&mut self) -> bool {
        self.poll_events();
        self.bulk_out_in_flight < BULK_BUF_COUNT
    }

    pub fn transmit(&mut self, packet: &[u8]) -> Result<()> {
        if packet.len() + 32 > BULK_BUFFER_SIZE {
            return Err(Error::msg("Packet exceeds USB Ethernet BULK_BUFFER_SIZE"));
        }
        if !self.can_transmit() {
            return Err(Error::msg("USB Ethernet Bulk-OUT ring is full"));
        }

        let idx = self.bulk_out_enqueue;
        let buf_idx = idx % BULK_BUF_COUNT;
        let cycle = self.bulk_out_cycle as u32;
        let buf_phys = self.tx_bufs.phys + (buf_idx * BULK_BUFFER_SIZE) as u64;
        let buf_slice = unsafe {
            core::slice::from_raw_parts_mut(
                self.tx_bufs.virt.add(buf_idx * BULK_BUFFER_SIZE),
                BULK_BUFFER_SIZE,
            )
        };

        let wire_len = match self.protocol {
            UsbNetProtocol::CdcEcm => {
                buf_slice[..packet.len()].copy_from_slice(packet);
                packet.len()
            }
            UsbNetProtocol::Rtl8153 => encode_rtl8153_tx_frame(packet, buf_slice),
            UsbNetProtocol::CdcNcm => {
                let seq = self.ncm_seq;
                self.ncm_seq = self.ncm_seq.wrapping_add(1);
                encode_cdc_ncm_tx_frame(seq, packet, buf_slice)
            }
        };

        fence(Ordering::SeqCst);

        Self::write_trb_raw(
            unsafe { self.ctrl_dma.virt.add(OFF_BULK_OUT_RING + idx * 16) },
            buf_phys,
            wire_len as u32,
            (TRB_NORMAL << 10) | (1 << 5) | cycle, // IOC = 1
        );
        self.bulk_out_enqueue += 1;
        if self.bulk_out_enqueue >= RING_SIZE - 1 {
            let link_addr = unsafe {
                self.ctrl_dma
                    .virt
                    .add(OFF_BULK_OUT_RING + (RING_SIZE - 1) * 16)
            };
            Self::write_trb_raw(
                link_addr,
                self.ctrl_dma.phys + OFF_BULK_OUT_RING as u64,
                0,
                (TRB_LINK << 10) | (1 << 1) | cycle,
            );
            self.bulk_out_enqueue = 0;
            self.bulk_out_cycle = !self.bulk_out_cycle;
        }

        self.bulk_out_in_flight += 1;
        self.ring_doorbell(self.bulk_out_dci);
        Ok(())
    }

    pub fn has_packets(&mut self) -> bool {
        self.poll_events();
        !self.rx_ready.is_empty()
    }

    pub fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        self.poll_events();
        let frame = self.rx_ready.pop_front()?;
        let copy_len = frame.len().min(buf.len());
        buf[..copy_len].copy_from_slice(&frame[..copy_len]);
        Some(copy_len)
    }
}

impl<H: Hal> Drop for UsbEthernetDevice<H> {
    fn drop(&mut self) {
        let cmd = self.hal.mmio_read32(self.op_base);
        if (cmd & 1) != 0 {
            self.hal.mmio_write32(self.op_base, cmd & !1);
        }
        self.hal.free_dma(self.ctrl_dma);
        self.hal.free_dma(self.rx_bufs);
        self.hal.free_dma(self.tx_bufs);
    }
}
