use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::ptr;
use core::sync::atomic::{Ordering, fence};

use crate::error::{Error, Result};
use crate::hal::{DmaBuffer, Hal, PAGE_SIZE, alloc_dma_buffers};
use crate::net::{is_valid_mac, resolve_mac};
use crate::xhci::{
    CTRL_BUF_SIZE, CTRL_DMA_PAGES, EventRing, IN_CTX_MAX_BYTES, OFF_CMD_RING, OFF_EP0_RING,
    OFF_IN_CTX, OFF_XFER_IN_RING, RING_SIZE, TRB_CONFIGURE_ENDPOINT, TRB_NORMAL,
    TRB_TRANSFER_EVENT, TrbRing, XhciCaps, XhciIo, XhciScratchpad, bios_handoff_and_reset,
    control_in, disable_slot, enable_and_address_slot, halt_controller, is_transfer_cc_ok,
    reset_and_probe_port, setup_and_start_controller, submit_command,
};

const BULK_BUF_COUNT: usize = 32;
pub const BULK_BUFFER_SIZE: usize = 2048;
const MAX_RX_READY: usize = 64;
const OFF_BULK_OUT_RING: usize = 0x0C00;

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

/// Iterates all valid Ethernet datagrams inside a 16-bit USB CDC-NCM Transfer Block (`NTH16` + `NDP16` chain).
fn for_each_cdc_ncm_datagram<'a>(ntb: &'a [u8], mut on_frame: impl FnMut(&'a [u8])) {
    if ntb.len() < 28 || &ntb[0..4] != b"NCMH" {
        return;
    }
    let mut ndp_idx = u16::from_le_bytes([ntb[10], ntb[11]]) as usize;
    for _ in 0..8 {
        if ndp_idx < 12 || ndp_idx + 16 > ntb.len() || &ntb[ndp_idx..ndp_idx + 3] != b"NCM" {
            break;
        }
        let ndp_len = u16::from_le_bytes([ntb[ndp_idx + 4], ntb[ndp_idx + 5]]) as usize;
        if ndp_len < 16 || ndp_idx + ndp_len > ntb.len() {
            break;
        }
        let next_ndp = u16::from_le_bytes([ntb[ndp_idx + 6], ntb[ndp_idx + 7]]) as usize;

        let mut pos = ndp_idx + 8;
        let end = ndp_idx + ndp_len;
        while pos + 4 <= end {
            let dg_idx = u16::from_le_bytes([ntb[pos], ntb[pos + 1]]) as usize;
            let dg_len = u16::from_le_bytes([ntb[pos + 2], ntb[pos + 3]]) as usize;
            if dg_idx == 0 && dg_len == 0 {
                break;
            }
            if dg_idx >= 12 && dg_len > 0 && dg_idx + dg_len <= ntb.len() {
                on_frame(&ntb[dg_idx..dg_idx + dg_len]);
            }
            pos += 4;
        }

        if next_ndp == 0 {
            break;
        }
        ndp_idx = next_ndp;
    }
}

/// Extracts the first Ethernet frame from a 16-bit USB CDC-NCM Transfer Block (`NTH16` + `NDP16`).
pub fn decode_cdc_ncm_rx_frame(ntb: &[u8]) -> Option<&[u8]> {
    let mut first = None;
    for_each_cdc_ncm_datagram(ntb, |frame| {
        if first.is_none() {
            first = Some(frame);
        }
    });
    first
}

pub struct UsbEthernetDevice<H: Hal> {
    pub mmio_base: u64,
    pub protocol: UsbNetProtocol,
    pub hal: H,
    op_off: u32,
    rt_off: u32,
    db_off: u32,
    ctrl_dma: DmaBuffer,
    rx_bufs: DmaBuffer,
    tx_bufs: DmaBuffer,
    scratchpad: Option<Box<XhciScratchpad>>,
    cmd_ring: TrbRing,
    ep0_ring: TrbRing,
    bulk_in_ring: TrbRing,
    bulk_out_ring: TrbRing,
    event_ring: EventRing,
    rx_buf_idx: u8,
    tx_buf_idx: u8,
    bulk_out_in_flight: u8,
    active_slot: u8,
    bulk_in_dci: u8,
    bulk_out_dci: u8,
    mac: [u8; 6],
    ncm_seq: u16,
    rx_ready: VecDeque<Vec<u8>>,
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    struct PtrHal(usize);
    impl Hal for PtrHal {
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
        fn phys_to_virt(&self, _: u64) -> *mut u8 {
            let _ = self.0;
            ptr::null_mut()
        }
        fn micros(&self) -> i64 {
            0
        }
        async fn yield_now(&self) {}
        async fn sleep(&self, _: core::time::Duration) {}
    }
    assert!(core::mem::size_of::<UsbEthernetDevice<PtrHal>>() <= 192);
};

impl<H: Hal> UsbEthernetDevice<H> {
    /// Initializes an xHCI controller at `mmio_base` and enumerates a connected USB Ethernet dongle
    /// (supporting USB CDC-ECM, CDC-NCM, and Realtek RTL8152/RTL8153/RTL8156, including composite USB docks):
    /// - Performs `USBLEGSUP` BIOS-to-OS handoff.
    /// - Resets and starts the xHCI controller with `DCBAA` (+ scratchpad buffers if requested),
    ///   Command Ring, Event Ring, EP0 Control Ring, Bulk-IN Ring, and Bulk-OUT Ring.
    /// - Queries USB Device and Configuration descriptors (up to 512 B) on connected ports to identify
    ///   the protocol (`Rtl8153`, `CdcEcm`, or `CdcNcm`), Bulk-IN / Bulk-OUT endpoints, and MAC address.
    /// - Queues Bulk-IN TRBs for non-blocking Ethernet packet reception.
    pub fn new(mmio_base: u64, fallback_mac: Option<[u8; 6]>, hal: H) -> Result<Self> {
        if mmio_base == 0 {
            return Err(Error::msg("USB Ethernet xHCI MMIO base is 0"));
        }
        let caps = XhciCaps::read_and_map(mmio_base, &hal)?;
        bios_handoff_and_reset(mmio_base, &caps, &hal)?;

        let bulk_pages = (BULK_BUF_COUNT * BULK_BUFFER_SIZE).div_ceil(PAGE_SIZE);
        let [ctrl_dma, rx_bufs, tx_bufs] = alloc_dma_buffers(
            &hal,
            [CTRL_DMA_PAGES, bulk_pages, bulk_pages],
            "Out of DMA memory for USB Ethernet",
        )?;
        unsafe {
            ptr::write_bytes(ctrl_dma.virt, 0, ctrl_dma.size);
        }

        let mut cmd_ring = TrbRing::new(OFF_CMD_RING);
        let mut ep0_ring = TrbRing::new(OFF_EP0_RING);
        let mut bulk_in_ring = TrbRing::new(OFF_XFER_IN_RING);
        let mut bulk_out_ring = TrbRing::new(OFF_BULK_OUT_RING);
        cmd_ring.init(&ctrl_dma);
        ep0_ring.init(&ctrl_dma);
        bulk_in_ring.init(&ctrl_dma);
        bulk_out_ring.init(&ctrl_dma);

        let scratchpad = match setup_and_start_controller(mmio_base, &caps, &ctrl_dma, &hal) {
            Ok(sp) => sp,
            Err(e) => {
                hal.free_dma(ctrl_dma);
                hal.free_dma(rx_bufs);
                hal.free_dma(tx_bufs);
                return Err(e);
            }
        };

        let mut dev = Self {
            mmio_base,
            protocol: UsbNetProtocol::CdcEcm,
            hal,
            op_off: caps.op_off,
            rt_off: caps.rt_off,
            db_off: caps.db_off,
            ctrl_dma,
            rx_bufs,
            tx_bufs,
            scratchpad,
            cmd_ring,
            ep0_ring,
            bulk_in_ring,
            bulk_out_ring,
            event_ring: EventRing::new(),
            rx_buf_idx: 0,
            tx_buf_idx: 0,
            bulk_out_in_flight: 0,
            active_slot: 0,
            bulk_in_dci: 3,
            bulk_out_dci: 4,
            mac: resolve_mac([0; 6], fallback_mac),
            ncm_seq: 0,
            rx_ready: VecDeque::new(),
        };

        let op_base = mmio_base + caps.op_off as u64;
        let mut initialized = false;
        for port_idx in 0..(caps.max_ports as usize) {
            let Some(speed) = reset_and_probe_port(op_base, port_idx, &dev.hal) else {
                continue;
            };
            if dev
                .init_port_nic(port_idx + 1, speed, caps.ctx_size as usize)
                .is_ok()
            {
                initialized = true;
                break;
            }
        }

        if !initialized {
            return Err(Error::msg("No USB Ethernet dongle found on xHCI ports"));
        }

        Ok(dev)
    }

    #[inline]
    fn op_base(&self) -> u64 {
        self.mmio_base + self.op_off as u64
    }

    #[inline]
    fn rt_base(&self) -> u64 {
        self.mmio_base + self.rt_off as u64
    }

    #[inline]
    fn db_base(&self) -> u64 {
        self.mmio_base + self.db_off as u64
    }

    fn ep0_control_in(&mut self, slot_id: u8, setup: [u8; 8], out: &mut [u8]) -> bool {
        let io = XhciIo {
            ctrl_dma: &self.ctrl_dma,
            rt_base: self.rt_base(),
            db_base: self.db_base(),
            hal: &self.hal,
        };
        control_in(
            &io,
            &mut self.ep0_ring,
            &mut self.event_ring,
            slot_id,
            setup,
            out,
        )
    }

    fn init_port_nic(&mut self, port_num: usize, speed: u8, ctx_size: usize) -> Result<()> {
        let slot_id = {
            let io = XhciIo {
                ctrl_dma: &self.ctrl_dma,
                rt_base: self.rt_base(),
                db_base: self.db_base(),
                hal: &self.hal,
            };
            enable_and_address_slot(
                &io,
                &mut self.cmd_ring,
                &mut self.ep0_ring,
                &mut self.event_ring,
                port_num,
                speed,
                ctx_size,
            )?
        };

        // 1. Query Device Descriptor (18 bytes) via EP0 Control Transfer
        let mut is_rtl8153 = false;
        let mut dev_class = 0u8;
        let mut dev_desc = [0u8; 18];
        if self.ep0_control_in(
            slot_id,
            [0x80, 0x06, 0x00, 0x01, 0x00, 0x00, 18, 0],
            &mut dev_desc,
        ) {
            let vid = u16::from_le_bytes([dev_desc[8], dev_desc[9]]);
            let pid = u16::from_le_bytes([dev_desc[10], dev_desc[11]]);
            dev_class = dev_desc[4];
            if vid == 0x0BDA && matches!(pid, 0x8152 | 0x8153 | 0x8156) {
                is_rtl8153 = true;
            }
        }

        // 2. Query Configuration Descriptor (up to 512 bytes) to discover NIC interface + Bulk IN/OUT pair.
        // Composite USB docks/dongles with additional HID or storage interfaces are supported!
        let mut proto = if is_rtl8153 {
            Some(UsbNetProtocol::Rtl8153)
        } else {
            None
        };
        let mut bulk_in: Option<u8> = None;
        let mut bulk_out: Option<u8> = None;

        let mut cfg_desc = [0u8; CTRL_BUF_SIZE];
        let len_le = (CTRL_BUF_SIZE as u16).to_le_bytes();
        if self.ep0_control_in(
            slot_id,
            [0x80, 0x06, 0x00, 0x02, 0x00, 0x00, len_le[0], len_le[1]],
            &mut cfg_desc,
        ) {
            let total_len =
                (u16::from_le_bytes([cfg_desc[2], cfg_desc[3]]) as usize).min(cfg_desc.len());
            let mut off = 0usize;
            let mut cur_iface_is_nic_data = false;

            while off + 2 <= total_len {
                let b_len = cfg_desc[off] as usize;
                let b_type = cfg_desc[off + 1];
                if b_len < 2 || off + b_len > total_len {
                    break;
                }
                if b_type == 4 && b_len >= 9 {
                    let class = cfg_desc[off + 5];
                    let subclass = cfg_desc[off + 6];
                    if class == 0x02 && subclass == 0x06 {
                        proto = Some(UsbNetProtocol::CdcEcm);
                        cur_iface_is_nic_data = true;
                    } else if class == 0x02 && subclass == 0x0D {
                        proto = Some(UsbNetProtocol::CdcNcm);
                        cur_iface_is_nic_data = true;
                    } else if class == 0x0A {
                        cur_iface_is_nic_data = true;
                    } else if class == 0xFF {
                        if proto.is_none() {
                            proto = Some(if is_rtl8153 {
                                UsbNetProtocol::Rtl8153
                            } else {
                                UsbNetProtocol::CdcEcm
                            });
                        }
                        cur_iface_is_nic_data = true;
                    } else {
                        // Ignore non-NIC interfaces (e.g. HID 0x03 or Mass Storage 0x08 on composite docks)
                        cur_iface_is_nic_data = false;
                    }
                } else if b_type == 5 && b_len >= 7 && cur_iface_is_nic_data {
                    let ep_addr = cfg_desc[off + 2];
                    let attr = cfg_desc[off + 3] & 0x03;
                    let ep_num = ep_addr & 0x0F;
                    if attr == 0x02 && ep_num != 0 {
                        let is_in = (ep_addr & 0x80) != 0;
                        let dci = (ep_num * 2 + u8::from(is_in)).clamp(2, 31);
                        if is_in && bulk_in.is_none() {
                            bulk_in = Some(dci);
                        } else if !is_in && bulk_out.is_none() {
                            bulk_out = Some(dci);
                        }
                    }
                }
                off += b_len;
            }
        } else if dev_class == 0x03 {
            let io = XhciIo {
                ctrl_dma: &self.ctrl_dma,
                rt_base: self.rt_base(),
                db_base: self.db_base(),
                hal: &self.hal,
            };
            disable_slot(&io, &mut self.cmd_ring, &mut self.event_ring, slot_id);
            return Err(Error::msg("USB device is HID, not Ethernet"));
        }

        let (Some(selected_proto), Some(in_dci), Some(out_dci)) = (proto, bulk_in, bulk_out) else {
            let io = XhciIo {
                ctrl_dma: &self.ctrl_dma,
                rt_base: self.rt_base(),
                db_base: self.db_base(),
                hal: &self.hal,
            };
            disable_slot(&io, &mut self.cmd_ring, &mut self.event_ring, slot_id);
            return Err(Error::msg(
                "No USB Ethernet interface with Bulk IN/OUT found",
            ));
        };

        self.active_slot = slot_id;
        self.protocol = selected_proto;
        self.bulk_in_dci = in_dci;
        self.bulk_out_dci = out_dci;
        self.bulk_in_ring.init(&self.ctrl_dma);
        self.bulk_out_ring.init(&self.ctrl_dma);
        self.rx_buf_idx = 0;
        self.tx_buf_idx = 0;
        self.bulk_out_in_flight = 0;

        // If Realtek RTL8152/8153, try reading 6-byte MAC from PLA IDR (`0xC000`, `wIndex = 0x0100`)
        if self.protocol == UsbNetProtocol::Rtl8153 {
            let mut pla_mac = [0u8; 8];
            if self.ep0_control_in(
                slot_id,
                [0xC0, 0x05, 0x00, 0xC0, 0x00, 0x01, 8, 0],
                &mut pla_mac,
            ) {
                let candidate: [u8; 6] = pla_mac[0..6].try_into().unwrap();
                if is_valid_mac(&candidate) {
                    self.mac = candidate;
                }
            }
        }

        // 3. Configure Bulk-IN and Bulk-OUT Endpoints in xHCI
        let max_dci = self.bulk_in_dci.max(self.bulk_out_dci) as u32;
        let bulk_max_pkt = if speed >= 4 { 1024u32 } else { 512u32 };
        let in_ctx_virt = unsafe { self.ctrl_dma.virt.add(OFF_IN_CTX) };
        let in_ctx_phys = self.ctrl_dma.phys + OFF_IN_CTX as u64;
        unsafe {
            ptr::write_bytes(in_ctx_virt, 0, IN_CTX_MAX_BYTES);
            let add_flags = (1u32 << 0) | (1u32 << self.bulk_in_dci) | (1u32 << self.bulk_out_dci);
            ptr::write_volatile(in_ctx_virt.add(4) as *mut u32, add_flags);

            let slot_ctx = in_ctx_virt.add(ctx_size) as *mut u32;
            ptr::write_volatile(slot_ctx, (max_dci << 27) | ((speed as u32) << 20));
            ptr::write_volatile(slot_ctx.add(1), (port_num as u32) << 16);

            // Bulk-IN Endpoint Context (EP Type = 6 = Bulk IN)
            let in_ep_ctx = in_ctx_virt.add((self.bulk_in_dci as usize + 1) * ctx_size) as *mut u32;
            let bulk_in_ring_phys = self.ctrl_dma.phys + OFF_XFER_IN_RING as u64;
            ptr::write_volatile(in_ep_ctx.add(1), (6 << 3) | (3 << 1) | (bulk_max_pkt << 16));
            ptr::write_volatile(in_ep_ctx.add(2), (bulk_in_ring_phys as u32) | 1);
            ptr::write_volatile(in_ep_ctx.add(3), (bulk_in_ring_phys >> 32) as u32);
            ptr::write_volatile(in_ep_ctx.add(4), BULK_BUFFER_SIZE as u32);

            // Bulk-OUT Endpoint Context (EP Type = 2 = Bulk OUT)
            let out_ep_ctx =
                in_ctx_virt.add((self.bulk_out_dci as usize + 1) * ctx_size) as *mut u32;
            let bulk_out_ring_phys = self.ctrl_dma.phys + OFF_BULK_OUT_RING as u64;
            ptr::write_volatile(
                out_ep_ctx.add(1),
                (2 << 3) | (3 << 1) | (bulk_max_pkt << 16),
            );
            ptr::write_volatile(out_ep_ctx.add(2), (bulk_out_ring_phys as u32) | 1);
            ptr::write_volatile(out_ep_ctx.add(3), (bulk_out_ring_phys >> 32) as u32);
            ptr::write_volatile(out_ep_ctx.add(4), BULK_BUFFER_SIZE as u32);
        }
        fence(Ordering::SeqCst);

        let io = XhciIo {
            ctrl_dma: &self.ctrl_dma,
            rt_base: self.rt_base(),
            db_base: self.db_base(),
            hal: &self.hal,
        };
        match submit_command(
            &io,
            &mut self.cmd_ring,
            &mut self.event_ring,
            in_ctx_phys,
            (TRB_CONFIGURE_ENDPOINT << 10) | ((slot_id as u32) << 24),
        ) {
            Some((1, _)) => {}
            _ => {
                self.active_slot = 0;
                disable_slot(&io, &mut self.cmd_ring, &mut self.event_ring, slot_id);
                return Err(Error::msg(
                    "xHCI Configure Endpoint failed for USB Ethernet",
                ));
            }
        }

        // 4. Queue initial Bulk-IN receive TRBs
        for _ in 0..16 {
            self.queue_bulk_in_trb();
        }
        self.ring_doorbell(self.bulk_in_dci);

        Ok(())
    }

    fn queue_bulk_in_trb(&mut self) {
        let buf_idx = self.rx_buf_idx as usize;
        self.rx_buf_idx = (self.rx_buf_idx + 1) % (BULK_BUF_COUNT as u8);
        let buf_phys = self.rx_bufs.phys + (buf_idx * BULK_BUFFER_SIZE) as u64;
        self.bulk_in_ring.enqueue(
            &self.ctrl_dma,
            buf_phys,
            BULK_BUFFER_SIZE as u32,
            (TRB_NORMAL << 10) | (1 << 5) | (1 << 2), // IOC = 1, ISP = 1
        );
    }

    fn ring_doorbell(&self, dci: u8) {
        let slot = self.active_slot.max(1) as u64;
        self.hal.mmio_write32(self.db_base() + slot * 4, dci as u32);
    }

    fn push_rx_frame(&mut self, frame: &[u8]) {
        if !frame.is_empty() && self.rx_ready.len() < MAX_RX_READY {
            self.rx_ready.push_back(frame.to_vec());
        }
    }

    fn poll_events(&mut self) {
        let rt_base = self.rt_base();
        let bulk_in_ring_phys = self.ctrl_dma.phys + OFF_XFER_IN_RING as u64;
        let bulk_out_ring_phys = self.ctrl_dma.phys + OFF_BULK_OUT_RING as u64;

        for _ in 0..RING_SIZE {
            let Some((trb_type, param, status, control)) =
                self.event_ring.pop(&self.ctrl_dma, rt_base, &self.hal)
            else {
                break;
            };
            if trb_type != TRB_TRANSFER_EVENT {
                continue;
            }
            let cc = ((status >> 24) & 0xFF) as u8;
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
                if is_transfer_cc_ok(cc) {
                    let (buf_phys, trb_len) = if param >= bulk_in_ring_phys
                        && param < bulk_in_ring_phys + (RING_SIZE * 16) as u64
                    {
                        let trb_idx = ((param - bulk_in_ring_phys) / 16) as usize;
                        let trb_virt =
                            unsafe { self.ctrl_dma.virt.add(OFF_XFER_IN_RING + trb_idx * 16) };
                        let p = unsafe { ptr::read_volatile(trb_virt as *const u64) };
                        let st = unsafe { ptr::read_volatile((trb_virt as *const u32).add(2)) };
                        (p, (st & 0x1FFFF) as usize)
                    } else {
                        (param, BULK_BUFFER_SIZE)
                    };

                    let transferred = trb_len.saturating_sub(residual).min(BULK_BUFFER_SIZE);
                    if transferred > 0
                        && buf_phys >= self.rx_bufs.phys
                        && buf_phys + transferred as u64
                            <= self.rx_bufs.phys + self.rx_bufs.size as u64
                    {
                        let off = (buf_phys - self.rx_bufs.phys) as usize;
                        let raw_slice = unsafe {
                            core::slice::from_raw_parts(self.rx_bufs.virt.add(off), transferred)
                        };
                        match self.protocol {
                            UsbNetProtocol::CdcEcm => self.push_rx_frame(raw_slice),
                            UsbNetProtocol::Rtl8153 => {
                                if let Some(frame) = decode_rtl8153_rx_frame(raw_slice) {
                                    self.push_rx_frame(frame);
                                }
                            }
                            UsbNetProtocol::CdcNcm => {
                                let rx_ready = &mut self.rx_ready;
                                for_each_cdc_ncm_datagram(raw_slice, |frame| {
                                    if !frame.is_empty() && rx_ready.len() < MAX_RX_READY {
                                        rx_ready.push_back(frame.to_vec());
                                    }
                                });
                            }
                        }
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
        (self.bulk_out_in_flight as usize) < BULK_BUF_COUNT
    }

    pub fn transmit(&mut self, packet: &[u8]) -> Result<()> {
        if packet.len() + 32 > BULK_BUFFER_SIZE {
            return Err(Error::msg("Packet exceeds USB Ethernet BULK_BUFFER_SIZE"));
        }
        if !self.can_transmit() {
            return Err(Error::msg("USB Ethernet Bulk-OUT ring is full"));
        }

        let buf_idx = self.tx_buf_idx as usize;
        self.tx_buf_idx = (self.tx_buf_idx + 1) % (BULK_BUF_COUNT as u8);
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

        fence(Ordering::Release);
        self.bulk_out_ring.enqueue(
            &self.ctrl_dma,
            buf_phys,
            wire_len as u32,
            (TRB_NORMAL << 10) | (1 << 5), // IOC = 1
        );

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
        halt_controller(self.op_base(), &self.hal);
        if let Some(sp) = self.scratchpad.take() {
            sp.free(&self.hal);
        }
        self.hal.free_dma(self.ctrl_dma);
        self.hal.free_dma(self.rx_bufs);
        self.hal.free_dma(self.tx_bufs);
    }
}
