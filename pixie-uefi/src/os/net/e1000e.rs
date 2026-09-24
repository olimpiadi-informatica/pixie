use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{Ordering, fence};

use crate::os::memory;
use crate::os::pci::{Bar, PciDevice};
use crate::os::raw_fb;

const NUM_RX_DESC: usize = 1024;
const NUM_TX_DESC: usize = 256;
pub const BUFFER_SIZE: usize = 2048;

// Intel Register Offsets
const REG_CTRL: usize = 0x0000;
const REG_STATUS: usize = 0x0008;
const REG_EERD: usize = 0x0014;
const REG_ICR: usize = 0x00C0;
const REG_IMC: usize = 0x00D8;
const REG_RCTL: usize = 0x0100;
const REG_TCTL: usize = 0x0400;
const REG_TIPG: usize = 0x0410;
const REG_RDBAL: usize = 0x2800;
const REG_RDBAH: usize = 0x2804;
const REG_RDLEN: usize = 0x2808;
const REG_RDH: usize = 0x2810;
const REG_RDT: usize = 0x2818;
const REG_RXDCTL: usize = 0x2828;
const REG_TDBAL: usize = 0x3800;
const REG_TDBAH: usize = 0x3804;
const REG_TDLEN: usize = 0x3808;
const REG_TDH: usize = 0x3810;
const REG_TDT: usize = 0x3818;
const REG_TXDCTL: usize = 0x3828;
const REG_TARC0: usize = 0x3840;
const REG_MTA: usize = 0x5200;
const REG_RAL: usize = 0x5400;
const REG_RAH: usize = 0x5404;
const REG_MANC: usize = 0x5820;
const REG_MANC2H: usize = 0x5860;
const REG_FWSM: usize = 0x5B54;
const REG_FEXTNVM11: usize = 0x5BBC;

// Control Register Bits
const CTRL_SLU: u32 = 1 << 6; // Set Link Up
const CTRL_RST: u32 = 1 << 26; // Device Reset

// Manageability and Errata Bits
const MANC_EN_MNG2HOST: u32 = 1 << 21; // Route management packets to host RX
const MANC_ARP_EN: u32 = 1 << 13;      // Hardware ARP interception
const FWSM_PCIM2PCI: u32 = 1 << 24;    // ME Host CSR contention
const FEXTNVM11_DISABLE_MULR_FIX: u32 = 1 << 13; // I219 MULR datapath hang erratum fix

#[derive(Clone, Copy, Debug, Default)]
pub struct E1000Stats {
    pub gprc: u32,
    pub gptc: u32,
    pub mpc: u32,
    pub rnbc: u32,
    pub crcerrs: u32,
    pub rdh: u32,
    pub rdt: u32,
    pub tdh: u32,
    pub tdt: u32,
    pub status: u32,
    pub tctl: u32,
    pub txdctl: u32,
    pub rctl: u32,
    pub rxdctl: u32,
}

// Receive Control Bits
const RCTL_EN: u32 = 1 << 1; // Receiver Enable
#[allow(dead_code)]
const RCTL_SBP: u32 = 1 << 2; // Store Bad Packets
const RCTL_UPE: u32 = 1 << 3; // Unicast Promiscuous Enable
const RCTL_MPE: u32 = 1 << 4; // Multicast Promiscuous Enable
const RCTL_BAM: u32 = 1 << 15; // Broadcast Accept Mode
const RCTL_SZ_2048: u32 = 0 << 16; // 2048 Byte Buffer Size
const RCTL_SECRC: u32 = 1 << 26; // Strip Ethernet CRC

// Receive Descriptor Control Bits
const RXDCTL_ENABLE: u32 = 1 << 25; // Queue Enable

// Transmit Control Bits
const TCTL_EN: u32 = 1 << 1; // Transmit Enable
const TCTL_PSP: u32 = 1 << 3; // Pad Short Packets
const TCTL_CT_SHIFT: u32 = 4; // Collision Threshold
const TCTL_COLD_SHIFT: u32 = 12; // Collision Distance
const TCTL_MULR: u32 = 1 << 28; // Multiple Request Support

// Transmit Descriptor Control Bits
const TXDCTL_PTHRESH_31: u32 = 0x1F; // Prefetch threshold: 31 descriptors
const TXDCTL_HTHRESH_1: u32 = 1 << 8; // Host threshold: 1 descriptor
const TXDCTL_WTHRESH_1: u32 = 1 << 16; // Writeback threshold: 1 descriptor
const TXDCTL_COUNT_DESC: u32 = 1 << 22; // Count descriptors
const TXDCTL_GRAN: u32 = 1 << 24; // Granularity = 1 (descriptors)
const TXDCTL_ENABLE: u32 = 1 << 25; // Queue Enable
const TXDCTL_DMA_BURST: u32 =
    TXDCTL_PTHRESH_31 | TXDCTL_HTHRESH_1 | TXDCTL_WTHRESH_1 | TXDCTL_COUNT_DESC | TXDCTL_GRAN;

// Transmit Command Bits
const CMD_EOP: u8 = 1 << 0; // End of Packet
const CMD_IFCS: u8 = 1 << 1; // Insert FCS/CRC
const CMD_RS: u8 = 1 << 3; // Report Status

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RxDesc {
    pub buffer_addr: u64,
    pub length: u16,
    pub checksum: u16,
    pub status: u8,
    pub errors: u8,
    pub special: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct TxDesc {
    pub buffer_addr: u64,
    pub length: u16,
    pub cso: u8,
    pub cmd: u8,
    pub status: u8,
    pub css: u8,
    pub special: u16,
}

#[allow(dead_code)]
pub struct E1000Device {
    pub pci: PciDevice,
    mmio_base: u64,
    mac: [u8; 6],
    rx_descs: &'static mut [RxDesc],
    rx_bufs: Vec<u64>,
    rx_cur: usize,
    tx_descs: &'static mut [TxDesc],
    tx_bufs: Vec<u64>,
    tx_cur: usize,
    tx_clean: usize,
}

impl E1000Device {
    pub fn new(pci: PciDevice) -> Result<Self, &'static str> {
        let mut w = raw_fb::StackWriter::<128>::new();
        let _ = core::write!(
            w,
            "[E1000] Init dev {:02x}:{:02x}.{:x} ({:04x}:{:04x})...",
            pci.bus,
            pci.dev,
            pci.func,
            pci.vendor_id,
            pci.device_id
        );
        raw_fb::print(w.as_str(), raw_fb::COLOR_CYAN, raw_fb::COLOR_DARK_BLUE);

        pci.enable_bus_mastering();

        let bar0 = pci.read_bar(0).ok_or("Failed to read BAR0")?;
        let mmio_base = match bar0 {
            Bar::Memory { base, .. } if base != 0 => base,
            _ => return Err("BAR0 is not valid MMIO"),
        };
        w.clear();
        let _ = core::write!(w, "[E1000] MMIO BAR0 = 0x{:X}", mmio_base);
        raw_fb::print(w.as_str(), raw_fb::COLOR_GREEN, raw_fb::COLOR_DARK_BLUE);

        // 1. Read MAC address before resetting the chip
        let mut mac = [0u8; 6];
        let ral = unsafe { Self::mmio_read(mmio_base, REG_RAL) };
        let rah = unsafe { Self::mmio_read(mmio_base, REG_RAH) };
        w.clear();
        let _ = core::write!(w, "[E1000] RAL=0x{:08X}, RAH=0x{:08X}", ral, rah);
        raw_fb::print(w.as_str(), raw_fb::COLOR_GREEN, raw_fb::COLOR_DARK_BLUE);

        let is_valid_mac = |m: &[u8; 6]| {
            *m != [0; 6] && *m != [0xFF; 6] && (m[0] & 1) == 0
        };

        if ral != 0 && ral != 0xFFFF_FFFF && (rah & (1 << 31)) != 0 {
            let candidate = [
                (ral & 0xFF) as u8,
                ((ral >> 8) & 0xFF) as u8,
                ((ral >> 16) & 0xFF) as u8,
                ((ral >> 24) & 0xFF) as u8,
                (rah & 0xFF) as u8,
                ((rah >> 8) & 0xFF) as u8,
            ];
            if is_valid_mac(&candidate) {
                mac = candidate;
            }
        }

        if !is_valid_mac(&mac) {
            w.clear();
            let _ = core::write!(w, "[E1000] Attempting EEPROM MAC read...");
            raw_fb::print(w.as_str(), raw_fb::COLOR_YELLOW, raw_fb::COLOR_DARK_BLUE);
            for i in 0..3 {
                let word = Self::read_eeprom(mmio_base, i as u8);
                mac[i * 2] = (word & 0xFF) as u8;
                mac[i * 2 + 1] = ((word >> 8) & 0xFF) as u8;
            }
        }

        if !is_valid_mac(&mac)
            && let Some(info) = *crate::os::boot_info::BOOT_INFO.lock()
            && let Some(uefi_mac) = info.uefi_mac
        {
            mac = uefi_mac;
            w.clear();
            let _ = core::write!(w, "[E1000] Fallback to UEFI PXE MAC");
            raw_fb::print(w.as_str(), raw_fb::COLOR_YELLOW, raw_fb::COLOR_DARK_BLUE);
        }

        w.clear();
        let _ = core::write!(
            w,
            "[E1000] MAC: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac[0],
            mac[1],
            mac[2],
            mac[3],
            mac[4],
            mac[5]
        );
        raw_fb::print(w.as_str(), raw_fb::COLOR_GREEN, raw_fb::COLOR_DARK_BLUE);

        // 2. Disable interrupts
        unsafe {
            Self::mmio_write(mmio_base, REG_IMC, 0xFFFF_FFFF);
            let _ = Self::mmio_read(mmio_base, REG_ICR);
        }

        // Diagnostic: log initial hardware state before stopping RX/TX
        let status_init = unsafe { Self::mmio_read(mmio_base, REG_STATUS) };
        let _rctl_init = unsafe { Self::mmio_read(mmio_base, REG_RCTL) };
        let _manc_init = unsafe { Self::mmio_read(mmio_base, REG_MANC) };
        let tctl_init = unsafe { Self::mmio_read(mmio_base, REG_TCTL) };
        let txdctl_init = unsafe { Self::mmio_read(mmio_base, REG_TXDCTL) };
        let tarc0_init = unsafe { Self::mmio_read(mmio_base, REG_TARC0) };
        let tipg_init = unsafe { Self::mmio_read(mmio_base, REG_TIPG) };
        w.clear();
        let _ = core::write!(
            w,
            "[E1000] Init STAT:0x{:X} TCTL:0x{:X} TXD:0x{:X} TARC0:0x{:X}",
            status_init,
            tctl_init,
            txdctl_init,
            tarc0_init
        );
        raw_fb::print(w.as_str(), raw_fb::COLOR_CYAN, raw_fb::COLOR_DARK_BLUE);

        // 3. Stop RX and TX units cleanly before setting up rings
        // NOTE: We deliberately do NOT assert CTRL_RST. On Intel PCH LAN controllers (I217/I218/I219),
        // asserting CTRL_RST causes internal interconnect stalls (hanging any subsequent MMIO read),
        // drops PHY autonegotiation, and conflicts with the Intel Management Engine (ME).
        // Since UEFI PXE has already initialized the PHY and negotiated link, a soft stop of RX/TX
        // cleanly resets descriptor processing without disrupting the hardware or link state.
        raw_fb::print(
            "[E1000] Stopping RX/TX units...",
            raw_fb::COLOR_CYAN,
            raw_fb::COLOR_DARK_BLUE,
        );
        unsafe {
            Self::mmio_write(mmio_base, REG_RCTL, 0);
            Self::mmio_write(mmio_base, REG_TCTL, 0);
            Self::mmio_write(mmio_base, REG_RDBAH, 0);
            Self::mmio_write(mmio_base, REG_RDBAL, 0);
            Self::mmio_write(mmio_base, REG_RDLEN, 0);
            Self::mmio_write(mmio_base, REG_RDH, 0);
            Self::safe_write_tail(mmio_base, REG_RDT, 0);
            Self::mmio_write(mmio_base, REG_RDBAH + 0x100, 0);
            Self::mmio_write(mmio_base, REG_RDBAL + 0x100, 0);
            Self::mmio_write(mmio_base, REG_RDLEN + 0x100, 0);
            Self::mmio_write(mmio_base, REG_RDH + 0x100, 0);
            Self::safe_write_tail(mmio_base, REG_RDT + 0x100, 0);
            Self::mmio_write(mmio_base, REG_RXDCTL + 0x100, 0);
            Self::mmio_write(mmio_base, REG_TDBAH, 0);
            Self::mmio_write(mmio_base, REG_TDBAL, 0);
            Self::mmio_write(mmio_base, REG_TDLEN, 0);
            Self::mmio_write(mmio_base, REG_TDH, 0);
            Self::safe_write_tail(mmio_base, REG_TDT, 0);
        }
        raw_fb::print(
            "[E1000] Stopped RX/TX units OK",
            raw_fb::COLOR_GREEN,
            raw_fb::COLOR_DARK_BLUE,
        );

        // 4. Disable interrupts again
        unsafe {
            Self::mmio_write(mmio_base, REG_IMC, 0xFFFF_FFFF);
            let _ = Self::mmio_read(mmio_base, REG_ICR);
        }

        // Apply I219 MULR datapath hang errata fix (FEXTNVM11 and TARC0/TARC1)
        let fextnvm11 = unsafe { Self::mmio_read(mmio_base, REG_FEXTNVM11) };
        if fextnvm11 != 0xFFFF_FFFF {
            unsafe {
                Self::mmio_write(
                    mmio_base,
                    REG_FEXTNVM11,
                    fextnvm11 | FEXTNVM11_DISABLE_MULR_FIX,
                );
            }
        }
        let tarc0 = unsafe { Self::mmio_read(mmio_base, REG_TARC0) };
        if tarc0 != 0xFFFF_FFFF {
            let tarc_val = (tarc0 & 0xCFFF_FFFF) | 0x2000_0000;
            unsafe {
                Self::mmio_write(mmio_base, REG_TARC0, tarc_val);
                Self::mmio_write(mmio_base, REG_TARC0 + 0x100, tarc_val);
            }
        }

        // Configure Intel AMT / ME packet forwarding so host RX ring receives DHCP and ARP packets
        let manc = unsafe { Self::mmio_read(mmio_base, REG_MANC) };
        if manc != 0xFFFF_FFFF {
            let new_manc = (manc | MANC_EN_MNG2HOST) & !MANC_ARP_EN;
            unsafe {
                Self::mmio_write(mmio_base, REG_MANC, new_manc);
                Self::mmio_write(mmio_base, REG_MANC2H, 0xFFFF_FFFF);
            }
            w.clear();
            let _ = core::write!(w, "[E1000] Configured MANC: 0x{:08X}", new_manc);
            raw_fb::print(w.as_str(), raw_fb::COLOR_GREEN, raw_fb::COLOR_DARK_BLUE);
        }

        // 5. Force link up and preserve control bits (do NOT set CTRL_ASDE on PCIe/PCH)
        let ctrl2 = unsafe { Self::mmio_read(mmio_base, REG_CTRL) };
        unsafe {
            Self::mmio_write(
                mmio_base,
                REG_CTRL,
                (ctrl2 & !CTRL_RST) | CTRL_SLU,
            );
        }

        // 6. Zero out Multicast Table Array (MTA)
        for i in 0..128 {
            unsafe {
                Self::mmio_write(mmio_base, REG_MTA + i * 4, 0);
            }
        }

        // 7. Restore MAC in Receive Address register
        let ral_val = (mac[0] as u32)
            | ((mac[1] as u32) << 8)
            | ((mac[2] as u32) << 16)
            | ((mac[3] as u32) << 24);
        let rah_val = (mac[4] as u32) | ((mac[5] as u32) << 8) | (1 << 31);
        unsafe {
            Self::mmio_write(mmio_base, REG_RAL, ral_val);
            Self::mmio_write(mmio_base, REG_RAH, rah_val);
        }
        raw_fb::print(
            "[E1000] MTA cleared & MAC written OK",
            raw_fb::COLOR_GREEN,
            raw_fb::COLOR_DARK_BLUE,
        );

        // 8. Allocate and initialize Receive Descriptor Ring
        let rx_ring_bytes = NUM_RX_DESC * core::mem::size_of::<RxDesc>();
        let rx_ring_pages = rx_ring_bytes.div_ceil(memory::PAGE_SIZE as usize);
        let rx_ring_phys =
            memory::alloc_contiguous(rx_ring_pages).ok_or("Out of memory for RX ring")?;
        let rx_descs =
            unsafe { core::slice::from_raw_parts_mut(rx_ring_phys as *mut RxDesc, NUM_RX_DESC) };

        let mut rx_bufs = alloc::vec![0u64; NUM_RX_DESC];
        for i in (0..NUM_RX_DESC).step_by(2) {
            let page = memory::alloc_page().ok_or("Out of memory for RX buffer")?;
            rx_bufs[i] = page;
            rx_bufs[i + 1] = page + (BUFFER_SIZE as u64);
        }

        for i in 0..NUM_RX_DESC {
            rx_descs[i] = RxDesc {
                buffer_addr: rx_bufs[i],
                length: 0,
                checksum: 0,
                status: 0,
                errors: 0,
                special: 0,
            };
        }

        unsafe {
            Self::mmio_write(
                mmio_base,
                REG_RDBAH,
                ((rx_ring_phys >> 32) & 0xFFFF_FFFF) as u32,
            );
            Self::mmio_write(mmio_base, REG_RDBAL, (rx_ring_phys & 0xFFFF_FFFF) as u32);
            Self::mmio_write(mmio_base, REG_RDLEN, rx_ring_bytes as u32);
            Self::mmio_write(mmio_base, REG_RDH, 0);
            Self::safe_write_tail(mmio_base, REG_RDT, (NUM_RX_DESC - 1) as u32);
        }

        const REG_MRQC: usize = 0x5818;
        let rctl = RCTL_EN | RCTL_UPE | RCTL_MPE | RCTL_BAM | RCTL_SZ_2048 | RCTL_SECRC;
        unsafe {
            // Disable Multiple Receive Queues / RSS so all incoming frames route directly to Queue 0
            Self::mmio_write(mmio_base, REG_MRQC, 0);

            Self::mmio_write(mmio_base, REG_RCTL, rctl);

            // Configure RXDCTL: WTHRESH=0 ensures immediate descriptor writeback upon packet arrival
            // without waiting for descriptor coalescing or interrupt delay timers (since we run polled).
            let mut rxdctl = Self::mmio_read(mmio_base, REG_RXDCTL);
            rxdctl &= !(0x3F | (0x3F << 8) | (0x3F << 16));
            rxdctl |= RXDCTL_ENABLE;
            Self::mmio_write(mmio_base, REG_RXDCTL, rxdctl);

            // Explicitly ensure Queue 1 is disabled so packets are not steered into an unmapped queue
            Self::mmio_write(mmio_base, REG_RXDCTL + 0x100, 0);
        }
        for _ in 0..10_000 {
            if (unsafe { Self::mmio_read(mmio_base, REG_RXDCTL) } & RXDCTL_ENABLE) != 0 {
                break;
            }
            core::hint::spin_loop();
        }
        raw_fb::print(
            "[E1000] RX ring configured & RCTL enabled OK",
            raw_fb::COLOR_GREEN,
            raw_fb::COLOR_DARK_BLUE,
        );

        // 9. Allocate and initialize Transmit Descriptor Ring
        let tx_ring_bytes = NUM_TX_DESC * core::mem::size_of::<TxDesc>();
        let tx_ring_pages = tx_ring_bytes.div_ceil(memory::PAGE_SIZE as usize);
        let tx_ring_phys =
            memory::alloc_contiguous(tx_ring_pages).ok_or("Out of memory for TX ring")?;
        let tx_descs =
            unsafe { core::slice::from_raw_parts_mut(tx_ring_phys as *mut TxDesc, NUM_TX_DESC) };

        let mut tx_bufs = alloc::vec![0u64; NUM_TX_DESC];
        for i in (0..NUM_TX_DESC).step_by(2) {
            let page = memory::alloc_page().ok_or("Out of memory for TX buffer")?;
            tx_bufs[i] = page;
            tx_bufs[i + 1] = page + (BUFFER_SIZE as u64);
        }

        for i in 0..NUM_TX_DESC {
            tx_descs[i] = TxDesc {
                buffer_addr: tx_bufs[i],
                length: 0,
                cso: 0,
                cmd: 0,
                status: 1, // DD=1: marked done / available
                css: 0,
                special: 0,
            };
        }

        unsafe {
            Self::mmio_write(
                mmio_base,
                REG_TDBAH,
                ((tx_ring_phys >> 32) & 0xFFFF_FFFF) as u32,
            );
            Self::mmio_write(mmio_base, REG_TDBAL, (tx_ring_phys & 0xFFFF_FFFF) as u32);
            Self::mmio_write(mmio_base, REG_TDLEN, tx_ring_bytes as u32);
            Self::mmio_write(mmio_base, REG_TDH, 0);
            Self::safe_write_tail(mmio_base, REG_TDT, 0);

            let tipg_val = if tipg_init != 0 && tipg_init != 0xFFFF_FFFF {
                tipg_init
            } else {
                10 | (8 << 10) | (6 << 20)
            };
            Self::mmio_write(mmio_base, REG_TIPG, tipg_val);

            // Configure TXDCTL: prefetch threshold PTHRESH=31 (0x1F), host threshold HTHRESH=1,
            // writeback threshold WTHRESH=1, count descriptors (COUNT_DESC=1), granularity (GRAN=1),
            // and queue enable (bit 25).
            // Crucial: When PTHRESH=0, hardware prefetch is disabled and descriptors in RAM are never
            // fetched, causing transmit to freeze (TDH=0, GPTC=0).
            let mut txdctl = Self::mmio_read(mmio_base, REG_TXDCTL);
            txdctl &= !(0x3F | (0x3F << 8) | (0x3F << 16));
            txdctl |= TXDCTL_DMA_BURST | TXDCTL_ENABLE;
            Self::mmio_write(mmio_base, REG_TXDCTL, txdctl);
            Self::mmio_write(mmio_base, REG_TXDCTL + 0x100, txdctl);
        }

        for _ in 0..10_000 {
            if (unsafe { Self::mmio_read(mmio_base, REG_TXDCTL) } & TXDCTL_ENABLE) != 0 {
                break;
            }
            core::hint::spin_loop();
        }

        // Configure TCTL: preserve UEFI PXE configuration if present, or set standard gigabit full-duplex
        let tctl = if tctl_init != 0 && tctl_init != 0xFFFF_FFFF {
            tctl_init | TCTL_EN | TCTL_PSP
        } else {
            TCTL_EN | TCTL_PSP | (0x0F << TCTL_CT_SHIFT) | (0x40 << TCTL_COLD_SHIFT) | TCTL_MULR
        };
        unsafe {
            Self::mmio_write(mmio_base, REG_TCTL, tctl);
        }
        let tctl_cur = unsafe { Self::mmio_read(mmio_base, REG_TCTL) };
        let txdctl_cur = unsafe { Self::mmio_read(mmio_base, REG_TXDCTL) };
        w.clear();
        let _ = core::write!(
            w,
            "[E1000] TX enabled (TCTL:0x{:X} TXDCTL:0x{:X})",
            tctl_cur,
            txdctl_cur
        );
        raw_fb::print(
            w.as_str(),
            raw_fb::COLOR_GREEN,
            raw_fb::COLOR_DARK_BLUE,
        );

        let mut status = unsafe { Self::mmio_read(mmio_base, REG_STATUS) };
        let mut link_up = (status & (1 << 1)) != 0;
        if !link_up {
            // Allow up to 1s for hardware to latch link status (STATUS.LU)
            for _ in 0..20 {
                let start = crate::os::timer::Timer::micros();
                while (crate::os::timer::Timer::micros() - start) < 50_000 {
                    core::hint::spin_loop();
                }
                status = unsafe { Self::mmio_read(mmio_base, REG_STATUS) };
                if (status & (1 << 1)) != 0 {
                    link_up = true;
                    break;
                }
            }
        }
        w.clear();
        let _ = core::write!(
            w,
            "[E1000] Status: 0x{:08X} (link: {})",
            status,
            if link_up { "UP" } else { "DOWN" }
        );
        raw_fb::print(
            w.as_str(),
            if link_up { raw_fb::COLOR_GREEN } else { raw_fb::COLOR_YELLOW },
            raw_fb::COLOR_DARK_BLUE,
        );

        Ok(Self {
            pci,
            mmio_base,
            mac,
            rx_descs,
            rx_bufs,
            rx_cur: 0,
            tx_descs,
            tx_bufs,
            tx_cur: 0,
            tx_clean: 0,
        })
    }

    pub fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    pub fn is_link_up(&self) -> bool {
        let status = unsafe { Self::mmio_read(self.mmio_base, REG_STATUS) };
        (status & (1 << 1)) != 0
    }

    fn clean_tx(&mut self) {
        while self.tx_clean != self.tx_cur {
            let desc = &self.tx_descs[self.tx_clean];
            let status = unsafe { core::ptr::read_volatile(&desc.status) };
            if (status & 1) != 0 {
                self.tx_clean = (self.tx_clean + 1) % NUM_TX_DESC;
            } else {
                break;
            }
        }
    }

    pub fn transmit(&mut self, packet: &[u8]) {
        assert!(packet.len() <= BUFFER_SIZE);

        self.clean_tx();
        let start = crate::os::timer::Timer::micros();
        while (self.tx_cur + 1) % NUM_TX_DESC == self.tx_clean {
            self.clean_tx();
            if (self.tx_cur + 1) % NUM_TX_DESC != self.tx_clean {
                break;
            }
            // Emergency fallback: if DD writeback is delayed by hardware, check TDH
            let tdh = (unsafe { Self::mmio_read(self.mmio_base, REG_TDH) } as usize) % NUM_TX_DESC;
            let advanced = if self.tx_cur >= self.tx_clean {
                tdh > self.tx_clean && tdh <= self.tx_cur
            } else {
                tdh > self.tx_clean || tdh <= self.tx_cur
            };
            if advanced {
                self.tx_clean = tdh;
                break;
            }
            core::hint::spin_loop();
            if (crate::os::timer::Timer::micros() - start) > 50_000 {
                let tdh = unsafe { Self::mmio_read(self.mmio_base, REG_TDH) };
                let tdt = unsafe { Self::mmio_read(self.mmio_base, REG_TDT) };
                log::error!(
                    "[E1000] Transmit timeout queue full (cur={}, clean={}, TDH={}, TDT={})",
                    self.tx_cur,
                    self.tx_clean,
                    tdh,
                    tdt
                );
                return;
            }
        }

        let desc = &mut self.tx_descs[self.tx_cur];
        let buf_ptr = self.tx_bufs[self.tx_cur] as *mut u8;
        unsafe {
            core::ptr::copy_nonoverlapping(packet.as_ptr(), buf_ptr, packet.len());
            core::ptr::write_volatile(&mut desc.buffer_addr, self.tx_bufs[self.tx_cur]);
            core::ptr::write_volatile(&mut desc.length, packet.len() as u16);
            core::ptr::write_volatile(&mut desc.cso, 0);
            core::ptr::write_volatile(&mut desc.cmd, CMD_EOP | CMD_IFCS | CMD_RS);
            core::ptr::write_volatile(&mut desc.status, 0);
            core::ptr::write_volatile(&mut desc.css, 0);
            core::ptr::write_volatile(&mut desc.special, 0);
        }
        fence(Ordering::SeqCst);

        self.tx_cur = (self.tx_cur + 1) % NUM_TX_DESC;
        unsafe {
            Self::safe_write_tail(self.mmio_base, REG_TDT, self.tx_cur as u32);
        }
    }

    pub fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        let desc = &mut self.rx_descs[self.rx_cur];
        let status = unsafe { core::ptr::read_volatile(&desc.status) };
        if (status & 1) == 0 {
            return None;
        }

        // Fatal errors: CRC Error (0x01), Symbol Error (0x02), RX Data Error (0x80)
        // Checksum flags (TCPE 0x20, IPE 0x40) are NOT fatal link errors; smoltcp verifies checksums in software.
        const RX_FATAL_ERRORS: u8 = 0x83;
        let errors = unsafe { core::ptr::read_volatile(&desc.errors) };
        if (errors & RX_FATAL_ERRORS) != 0 || (status & 2) == 0 {
            unsafe {
                core::ptr::write_volatile(&mut desc.buffer_addr, self.rx_bufs[self.rx_cur]);
                core::ptr::write_volatile(&mut desc.status, 0);
            }
            fence(Ordering::SeqCst);
            let old_cur = self.rx_cur;
            self.rx_cur = (self.rx_cur + 1) % NUM_RX_DESC;
            unsafe {
                Self::safe_write_tail(self.mmio_base, REG_RDT, old_cur as u32);
            }
            return None;
        }

        fence(Ordering::SeqCst);
        let length = unsafe { core::ptr::read_volatile(&desc.length) } as usize;
        let copy_len = length.min(buf.len());
        let buf_ptr = self.rx_bufs[self.rx_cur] as *const u8;
        unsafe {
            core::ptr::copy_nonoverlapping(buf_ptr, buf.as_mut_ptr(), copy_len);
            core::ptr::write_volatile(&mut desc.buffer_addr, self.rx_bufs[self.rx_cur]);
            core::ptr::write_volatile(&mut desc.status, 0);
        }
        fence(Ordering::SeqCst);

        let old_cur = self.rx_cur;
        self.rx_cur = (self.rx_cur + 1) % NUM_RX_DESC;
        unsafe {
            Self::safe_write_tail(self.mmio_base, REG_RDT, old_cur as u32);
        }

        Some(copy_len)
    }

    pub fn has_packets(&self) -> bool {
        let desc = &self.rx_descs[self.rx_cur];
        let status = unsafe { core::ptr::read_volatile(&desc.status) };
        (status & 1) != 0
    }

    pub fn can_transmit(&mut self) -> bool {
        self.clean_tx();
        (self.tx_cur + 1) % NUM_TX_DESC != self.tx_clean
    }

    pub fn read_stats(&self) -> E1000Stats {
        const REG_CRCERRS: usize = 0x4000;
        const REG_MPC: usize = 0x4010;
        const REG_GPRC: usize = 0x4074;
        const REG_GPTC: usize = 0x4080;
        const REG_RNBC: usize = 0x40A0;

        unsafe {
            E1000Stats {
                gprc: Self::mmio_read(self.mmio_base, REG_GPRC),
                gptc: Self::mmio_read(self.mmio_base, REG_GPTC),
                mpc: Self::mmio_read(self.mmio_base, REG_MPC),
                rnbc: Self::mmio_read(self.mmio_base, REG_RNBC),
                crcerrs: Self::mmio_read(self.mmio_base, REG_CRCERRS),
                rdh: Self::mmio_read(self.mmio_base, REG_RDH),
                rdt: Self::mmio_read(self.mmio_base, REG_RDT),
                tdh: Self::mmio_read(self.mmio_base, REG_TDH),
                tdt: Self::mmio_read(self.mmio_base, REG_TDT),
                status: Self::mmio_read(self.mmio_base, REG_STATUS),
                tctl: Self::mmio_read(self.mmio_base, REG_TCTL),
                txdctl: Self::mmio_read(self.mmio_base, REG_TXDCTL),
                rctl: Self::mmio_read(self.mmio_base, REG_RCTL),
                rxdctl: Self::mmio_read(self.mmio_base, REG_RXDCTL),
            }
        }
    }

    unsafe fn safe_write_tail(base: u64, reg: usize, val: u32) {
        // Wait for ME firmware to release CSR access (FWSM bit 24)
        for _ in 0..2000 {
            if unsafe { (Self::mmio_read(base, REG_FWSM) & FWSM_PCIM2PCI) == 0 } {
                break;
            }
            let start = crate::os::timer::Timer::micros();
            while (crate::os::timer::Timer::micros() - start) < 50 {
                core::hint::spin_loop();
            }
        }

        unsafe {
            Self::mmio_write(base, reg, val);

            // Verify write was accepted, retrying if necessary (as done in Linux e1000e_update_rdt_wa)
            for _ in 0..1000 {
                if Self::mmio_read(base, reg) == val {
                    return;
                }
                Self::mmio_write(base, reg, val);
                core::hint::spin_loop();
            }
        }
    }

    unsafe fn mmio_read(base: u64, offset: usize) -> u32 {
        unsafe { core::ptr::read_volatile((base + offset as u64) as *const u32) }
    }

    unsafe fn mmio_write(base: u64, offset: usize, val: u32) {
        unsafe { core::ptr::write_volatile((base + offset as u64) as *mut u32, val) }
    }

    fn read_eeprom(base: u64, addr: u8) -> u16 {
        // Try e1000 format: start=bit 0, addr=bits 8..15, done=bit 4, data=bits 16..31
        unsafe {
            Self::mmio_write(base, REG_EERD, 1 | ((addr as u32) << 8));
        }
        for _ in 0..10_000 {
            let val = unsafe { Self::mmio_read(base, REG_EERD) };
            if (val & (1 << 4)) != 0 {
                return (val >> 16) as u16;
            }
            core::hint::spin_loop();
        }

        // Try e1000e format: start=bit 0, addr=bits 2..15, done=bit 1, data=bits 16..31
        unsafe {
            Self::mmio_write(base, REG_EERD, 1 | ((addr as u32) << 2));
        }
        for _ in 0..10_000 {
            let val = unsafe { Self::mmio_read(base, REG_EERD) };
            if (val & (1 << 1)) != 0 {
                return (val >> 16) as u16;
            }
            core::hint::spin_loop();
        }
        0
    }
}

impl Drop for E1000Device {
    fn drop(&mut self) {
        unsafe {
            // Disable RX, TX and all interrupts
            Self::mmio_write(self.mmio_base, REG_RCTL, 0);
            Self::mmio_write(self.mmio_base, REG_TCTL, 0);
            Self::mmio_write(self.mmio_base, REG_IMC, 0xFFFF_FFFF);
            let _ = Self::mmio_read(self.mmio_base, REG_ICR);
        }

        let rx_ring_bytes = NUM_RX_DESC * core::mem::size_of::<RxDesc>();
        let rx_ring_pages = rx_ring_bytes.div_ceil(memory::PAGE_SIZE as usize);
        memory::free_contiguous(self.rx_descs.as_mut_ptr() as u64, rx_ring_pages);

        let tx_ring_bytes = NUM_TX_DESC * core::mem::size_of::<TxDesc>();
        let tx_ring_pages = tx_ring_bytes.div_ceil(memory::PAGE_SIZE as usize);
        memory::free_contiguous(self.tx_descs.as_mut_ptr() as u64, tx_ring_pages);

        for &page in self.rx_bufs.iter().step_by(2) {
            memory::free_page(page);
        }
        for &page in self.tx_bufs.iter().step_by(2) {
            memory::free_page(page);
        }
    }
}

pub fn probe(pci: &PciDevice) -> bool {
    // Must be Intel, Network Controller (0x02), and Ethernet (0x00)
    if pci.vendor_id != 0x8086 || pci.class_code != 0x02 || pci.subclass != 0x00 {
        return false;
    }
    // Explicitly exclude known Intel Wireless Wi-Fi device IDs (e.g. 0x24F3 Wireless 8260)
    if pci.device_id == 0x24f3 || pci.device_id == 0x24f4 || (pci.device_id & 0xFF00 == 0x2400) {
        return false;
    }
    true
}
