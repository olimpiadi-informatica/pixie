use core::sync::atomic::{Ordering, fence};

use crate::error::{Error, Result};
use crate::hal::Hal;
use crate::net::intel::{IntelRings, read_ral_rah_mac, write_mta_and_rar0};
use crate::net::{is_valid_mac, resolve_mac};
use crate::pci::PciDevice;

pub use crate::net::intel::{BUFFER_SIZE, NUM_RX_DESC, NUM_TX_DESC, RxDesc, TxDesc};

const REG_CTRL: u64 = 0x0000;
const REG_STATUS: u64 = 0x0008;
const REG_EERD: u64 = 0x0014;
const REG_ICR: u64 = 0x00C0;
const REG_IMC: u64 = 0x00D8;
const REG_RCTL: u64 = 0x0100;
const REG_TCTL: u64 = 0x0400;
const REG_TIPG: u64 = 0x0410;
const REG_EXTCNF_CTRL: u64 = 0x0F00;
const REG_RDBAL: u64 = 0x2800;
const REG_RDBAH: u64 = 0x2804;
const REG_RDLEN: u64 = 0x2808;
const REG_RDH: u64 = 0x2810;
const REG_RDT: u64 = 0x2818;
const REG_RXDCTL: u64 = 0x2828;
const REG_TDBAL: u64 = 0x3800;
const REG_TDBAH: u64 = 0x3804;
const REG_TDLEN: u64 = 0x3808;
const REG_TDH: u64 = 0x3810;
const REG_TDT: u64 = 0x3818;
const REG_TXDCTL: u64 = 0x3828;
const REG_TARC0: u64 = 0x3840;
const REG_CRCERRS: u64 = 0x4000;
const REG_MPC: u64 = 0x4010;
const REG_GPRC: u64 = 0x4074;
const REG_GPTC: u64 = 0x4080;
const REG_RNBC: u64 = 0x40A0;
const REG_RFCTL: u64 = 0x5008;
const REG_MTA: u64 = 0x5200;
const REG_RAL: u64 = 0x5400;
const REG_RAH: u64 = 0x5404;
const REG_MRQC: u64 = 0x5818;
const REG_MANC: u64 = 0x5820;
const REG_MANC2H: u64 = 0x5860;
const REG_FWSM: u64 = 0x5B54;
const REG_FEXTNVM11: u64 = 0x5BBC;

const CTRL_SLU: u32 = 1 << 6;
const CTRL_RST: u32 = 1 << 26;

const RFCTL_EXSTEN: u32 = 1 << 15;
const MANC_EN_MNG2HOST: u32 = 1 << 21;
const MANC_ARP_EN: u32 = 1 << 13;
const FWSM_PCIM2PCI: u32 = 1 << 24;
const FEXTNVM11_DISABLE_MULR_FIX: u32 = 1 << 13;

const RCTL_EN: u32 = 1 << 1;
const RCTL_UPE: u32 = 1 << 3;
const RCTL_MPE: u32 = 1 << 4;
const RCTL_BAM: u32 = 1 << 15;
const RCTL_SZ_2048: u32 = 0 << 16;
const RCTL_SECRC: u32 = 1 << 26;

const RXDCTL_ENABLE: u32 = 1 << 25;

const TCTL_EN: u32 = 1 << 1;
const TCTL_PSP: u32 = 1 << 3;
const TCTL_CT_SHIFT: u32 = 4;
const TCTL_COLD_SHIFT: u32 = 12;
const TCTL_MULR: u32 = 1 << 28;

const TXDCTL_PTHRESH_31: u32 = 0x1F;
const TXDCTL_HTHRESH_1: u32 = 1 << 8;
const TXDCTL_WTHRESH_1: u32 = 1 << 16;
const TXDCTL_COUNT_DESC: u32 = 1 << 22;
const TXDCTL_GRAN: u32 = 1 << 24;
const TXDCTL_ENABLE: u32 = 1 << 25;
const TXDCTL_DMA_BURST: u32 =
    TXDCTL_PTHRESH_31 | TXDCTL_HTHRESH_1 | TXDCTL_WTHRESH_1 | TXDCTL_COUNT_DESC | TXDCTL_GRAN;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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

pub struct E1000Device<H: Hal> {
    pub pci: PciDevice,
    pub mmio_base: u64,
    pub hal: H,
    mac: [u8; 6],
    rings: IntelRings,
}

impl<H: Hal> E1000Device<H> {
    fn mmio_read(&self, offset: u64) -> u32 {
        self.hal.mmio_read32(self.mmio_base + offset)
    }

    fn mmio_write(&self, offset: u64, val: u32) {
        self.hal.mmio_write32(self.mmio_base + offset, val);
    }

    fn safe_write_tail(hal: &H, base: u64, reg: u64, val: u32) {
        for _ in 0..2000 {
            let fwsm = hal.mmio_read32(base + REG_FWSM);
            if fwsm == 0xFFFF_FFFF || (fwsm & FWSM_PCIM2PCI) == 0 {
                break;
            }
        }
        hal.mmio_write32(base + reg, val);
        for _ in 0..1000 {
            if hal.mmio_read32(base + reg) == val {
                return;
            }
            hal.mmio_write32(base + reg, val);
        }
    }

    fn read_eeprom(hal: &H, base: u64, addr: u8) -> u16 {
        // e1000 format: start=bit 0, addr=bits 8..15, done=bit 4, data=bits 16..31
        hal.mmio_write32(base + REG_EERD, 1 | ((addr as u32) << 8));
        for _ in 0..1_000 {
            let val = hal.mmio_read32(base + REG_EERD);
            if val != 0xFFFF_FFFF && (val & (1 << 4)) != 0 {
                return (val >> 16) as u16;
            }
        }

        // e1000e format: start=bit 0, addr=bits 2..15, done=bit 1, data=bits 16..31
        hal.mmio_write32(base + REG_EERD, 1 | ((addr as u32) << 2));
        for _ in 0..1_000 {
            let val = hal.mmio_read32(base + REG_EERD);
            if val != 0xFFFF_FFFF && (val & (1 << 1)) != 0 {
                return (val >> 16) as u16;
            }
        }
        0
    }

    /// Initializes an Intel e1000 / e1000e (82540EM, 82574L, I217, I218, I219) Ethernet controller:
    /// - Reads MAC from RAL/RAH, falling back to EEPROM (`EERD`), `fallback_mac`, or locally-administered default.
    /// - Disables interrupts (`IMC = 0xFFFF_FFFF`), stops RX/TX cleanly without asserting destructive `CTRL.RST`.
    /// - Clears `RFCTL` (`0x5008`, disabling `EXSTEN` extended descriptors) and `MRQC` (`0x5818`, disabling multi-queue RSS).
    /// - Configures `MANC` (`0x5820`, setting `EN_MNG2HOST` and clearing `ARP_EN`) and `FEXTNVM11` (`0x5BBC`).
    /// - Initializes RX ring in strict order: `RCTL=0` -> program `RDBAL/H`, `RDLEN`, `RDH=0`, `RDT=0` ->
    ///   enable `RXDCTL` (`ENABLE | PTHRESH(16) | HTHRESH(4) | WTHRESH(1)`) -> enable `RCTL` -> write `RDT = NUM_RX_DESC - 1`
    ///   (waiting for `FWSM.PCIM2PCI == 0` and verifying tail writeback).
    /// - Initializes TX ring (`TXDCTL` with `PTHRESH/HTHRESH/GRAN/ENABLE`, `TCTL` with `EN | PSP`, clearing `MULR`).
    pub fn new(
        pci: PciDevice,
        mmio_base: u64,
        fallback_mac: Option<[u8; 6]>,
        hal: H,
    ) -> Result<Self> {
        if mmio_base == 0 {
            return Err(Error::msg("E1000 MMIO base is 0"));
        }
        pci.enable_bus_mastering(&hal);
        hal.map_mmio(mmio_base, 0x20000);

        if hal.mmio_read32(mmio_base + REG_STATUS) == 0xFFFF_FFFF {
            return Err(Error::msg("E1000 MMIO read returned 0xFFFFFFFF"));
        }

        // Release PCH EXTCNF_CTRL SWFLAG semaphore if held
        let extcnf = hal.mmio_read32(mmio_base + REG_EXTCNF_CTRL);
        if extcnf != 0xFFFF_FFFF && (extcnf & (1 << 5)) != 0 {
            hal.mmio_write32(mmio_base + REG_EXTCNF_CTRL, extcnf & !(1 << 5));
        }

        // 1. Read MAC address from RAL/RAH, falling back to EEPROM (EERD) and then fallback_mac
        let mut hw_mac = read_ral_rah_mac(&hal, mmio_base, REG_RAL, REG_RAH);
        if !is_valid_mac(&hw_mac) {
            let mut eeprom_mac = [0u8; 6];
            for i in 0..3 {
                let word = Self::read_eeprom(&hal, mmio_base, i as u8);
                eeprom_mac[i * 2] = (word & 0xFF) as u8;
                eeprom_mac[i * 2 + 1] = ((word >> 8) & 0xFF) as u8;
            }
            if is_valid_mac(&eeprom_mac) {
                hw_mac = eeprom_mac;
            }
        }
        let mac = resolve_mac(hw_mac, fallback_mac);

        // 2. Disable interrupts
        hal.mmio_write32(mmio_base + REG_IMC, 0xFFFF_FFFF);
        let _ = hal.mmio_read32(mmio_base + REG_ICR);

        let tctl_init = hal.mmio_read32(mmio_base + REG_TCTL);
        let tipg_init = hal.mmio_read32(mmio_base + REG_TIPG);

        // 3. Stop RX and TX cleanly without asserting destructive CTRL.RST
        hal.mmio_write32(mmio_base + REG_RCTL, 0);
        hal.mmio_write32(mmio_base + REG_TCTL, 0);
        hal.mmio_write32(mmio_base + REG_RDBAH, 0);
        hal.mmio_write32(mmio_base + REG_RDBAL, 0);
        hal.mmio_write32(mmio_base + REG_RDLEN, 0);
        hal.mmio_write32(mmio_base + REG_RDH, 0);
        Self::safe_write_tail(&hal, mmio_base, REG_RDT, 0);
        hal.mmio_write32(mmio_base + REG_RDBAH + 0x100, 0);
        hal.mmio_write32(mmio_base + REG_RDBAL + 0x100, 0);
        hal.mmio_write32(mmio_base + REG_RDLEN + 0x100, 0);
        hal.mmio_write32(mmio_base + REG_RDH + 0x100, 0);
        Self::safe_write_tail(&hal, mmio_base, REG_RDT + 0x100, 0);
        hal.mmio_write32(mmio_base + REG_RXDCTL + 0x100, 0);
        hal.mmio_write32(mmio_base + REG_TDBAH, 0);
        hal.mmio_write32(mmio_base + REG_TDBAL, 0);
        hal.mmio_write32(mmio_base + REG_TDLEN, 0);
        hal.mmio_write32(mmio_base + REG_TDH, 0);
        Self::safe_write_tail(&hal, mmio_base, REG_TDT, 0);

        hal.mmio_write32(mmio_base + REG_IMC, 0xFFFF_FFFF);
        let _ = hal.mmio_read32(mmio_base + REG_ICR);

        // 4. Clear RFCTL (disabling EXSTEN) and MRQC (disabling multi-queue RSS)
        let rfctl = hal.mmio_read32(mmio_base + REG_RFCTL);
        let new_rfctl = if rfctl == 0xFFFF_FFFF {
            0
        } else {
            rfctl & !RFCTL_EXSTEN
        };
        hal.mmio_write32(mmio_base + REG_RFCTL, new_rfctl);
        hal.mmio_write32(mmio_base + REG_MRQC, 0);

        // 5. Configure FEXTNVM11, TARC0, and MANC
        let fextnvm11 = hal.mmio_read32(mmio_base + REG_FEXTNVM11);
        if fextnvm11 != 0xFFFF_FFFF {
            hal.mmio_write32(
                mmio_base + REG_FEXTNVM11,
                fextnvm11 & !FEXTNVM11_DISABLE_MULR_FIX,
            );
        }
        let tarc0 = hal.mmio_read32(mmio_base + REG_TARC0);
        if tarc0 != 0xFFFF_FFFF {
            let tarc_val = (tarc0 & 0xCFFF_FFFF) | 0x2000_0000;
            hal.mmio_write32(mmio_base + REG_TARC0, tarc_val);
            hal.mmio_write32(mmio_base + REG_TARC0 + 0x100, tarc_val);
        }
        let manc = hal.mmio_read32(mmio_base + REG_MANC);
        if manc != 0xFFFF_FFFF {
            let new_manc = (manc | MANC_EN_MNG2HOST) & !MANC_ARP_EN;
            hal.mmio_write32(mmio_base + REG_MANC, new_manc);
            hal.mmio_write32(mmio_base + REG_MANC2H, 0xFFFF_FFFF);
        }

        // 6. Force link up in CTRL without asserting CTRL_RST
        let ctrl = hal.mmio_read32(mmio_base + REG_CTRL);
        let ctrl_base = if ctrl == 0xFFFF_FFFF { 0 } else { ctrl };
        hal.mmio_write32(mmio_base + REG_CTRL, (ctrl_base & !CTRL_RST) | CTRL_SLU);

        // 7. Clear Multicast Table Array (MTA) and program RAL0/RAH0
        write_mta_and_rar0(&hal, mmio_base, REG_MTA, REG_RAL, REG_RAH, mac);

        // 8. Allocate DMA memory for RX and TX rings + buffers
        let mut rings = IntelRings::alloc(&hal, "Out of DMA memory for e1000")?;

        // 9. Initialize RX ring in strict order
        hal.mmio_write32(mmio_base + REG_RCTL, 0);
        hal.mmio_write32(mmio_base + REG_RDBAH, (rings.rx_ring.phys >> 32) as u32);
        hal.mmio_write32(mmio_base + REG_RDBAL, rings.rx_ring.phys as u32);
        hal.mmio_write32(mmio_base + REG_RDLEN, IntelRings::RX_RING_BYTES as u32);
        hal.mmio_write32(mmio_base + REG_RDH, 0);
        hal.mmio_write32(mmio_base + REG_RDT, 0);

        let rxdctl_orig = hal.mmio_read32(mmio_base + REG_RXDCTL);
        let rxdctl_base = if rxdctl_orig == 0xFFFF_FFFF {
            0
        } else {
            rxdctl_orig & !(0x3F | (0x3F << 8) | (0x3F << 16))
        };
        let rxdctl = rxdctl_base | 16 | (4 << 8) | (1 << 16) | RXDCTL_ENABLE;
        hal.mmio_write32(mmio_base + REG_RXDCTL, rxdctl);
        hal.mmio_write32(mmio_base + REG_RXDCTL + 0x100, 0);

        for _ in 0..1_000 {
            if (hal.mmio_read32(mmio_base + REG_RXDCTL) & RXDCTL_ENABLE) != 0 {
                break;
            }
        }

        let rctl = RCTL_EN | RCTL_UPE | RCTL_MPE | RCTL_BAM | RCTL_SZ_2048 | RCTL_SECRC;
        hal.mmio_write32(mmio_base + REG_RCTL, rctl);
        fence(Ordering::SeqCst);
        Self::safe_write_tail(&hal, mmio_base, REG_RDT, (NUM_RX_DESC - 1) as u32);

        // 10. Initialize TX ring
        hal.mmio_write32(mmio_base + REG_TDBAH, (rings.tx_ring.phys >> 32) as u32);
        hal.mmio_write32(mmio_base + REG_TDBAL, rings.tx_ring.phys as u32);
        hal.mmio_write32(mmio_base + REG_TDLEN, IntelRings::TX_RING_BYTES as u32);
        hal.mmio_write32(mmio_base + REG_TDH, 0);
        Self::safe_write_tail(&hal, mmio_base, REG_TDT, 0);

        let tipg_val = if tipg_init != 0 && tipg_init != 0xFFFF_FFFF {
            tipg_init
        } else {
            10 | (8 << 10) | (6 << 20)
        };
        hal.mmio_write32(mmio_base + REG_TIPG, tipg_val);

        let txdctl_orig = hal.mmio_read32(mmio_base + REG_TXDCTL);
        let txdctl_base = if txdctl_orig == 0xFFFF_FFFF {
            0
        } else {
            txdctl_orig & !(0x3F | (0x3F << 8) | (0x3F << 16))
        };
        let txdctl = txdctl_base | TXDCTL_DMA_BURST | TXDCTL_ENABLE;
        hal.mmio_write32(mmio_base + REG_TXDCTL, txdctl);
        hal.mmio_write32(mmio_base + REG_TXDCTL + 0x100, txdctl);

        for _ in 0..1_000 {
            if (hal.mmio_read32(mmio_base + REG_TXDCTL) & TXDCTL_ENABLE) != 0 {
                break;
            }
        }

        let tctl = if tctl_init != 0 && tctl_init != 0xFFFF_FFFF {
            (tctl_init & !TCTL_MULR) | TCTL_EN | TCTL_PSP
        } else {
            TCTL_EN | TCTL_PSP | (0x0F << TCTL_CT_SHIFT) | (0x40 << TCTL_COLD_SHIFT)
        };
        hal.mmio_write32(mmio_base + REG_TCTL, tctl);

        let hw_tdh = rings.sync_hw_tdh(hal.mmio_read32(mmio_base + REG_TDH));
        if hw_tdh != 0 {
            Self::safe_write_tail(&hal, mmio_base, REG_TDT, hw_tdh as u32);
        }

        Ok(Self {
            pci,
            mmio_base,
            hal,
            mac,
            rings,
        })
    }

    pub fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    pub fn is_link_up(&self) -> bool {
        let status = self.mmio_read(REG_STATUS);
        status != 0xFFFF_FFFF && (status & (1 << 1)) != 0
    }

    /// Reclaims completed TX descriptors (via `TxDesc.status & 1` or hardware `REG_TDH` fallback)
    /// and returns `true` if at least one TX descriptor slot is free.
    pub fn can_transmit(&mut self) -> bool {
        let raw_tdh = self.mmio_read(REG_TDH);
        self.rings.can_transmit(raw_tdh)
    }

    /// Submits `packet` to the TX ring if `can_transmit()` is true. Never spin-waits or resets the TX ring.
    pub fn transmit(&mut self, packet: &[u8]) -> Result<()> {
        let raw_tdh = self.mmio_read(REG_TDH);
        let hal = &self.hal;
        let base = self.mmio_base;
        self.rings.transmit(
            packet,
            raw_tdh,
            "Packet exceeds e1000 BUFFER_SIZE",
            "E1000 TX ring is full",
            |tdt| Self::safe_write_tail(hal, base, REG_TDT, tdt),
        )
    }

    /// Returns `true` if the current RX descriptor has `DD` (`status & 1`) set.
    pub fn has_packets(&self) -> bool {
        self.rings.has_packets()
    }

    /// Receives the next packet into `buf`, replenishes the RX descriptor (`status = 0`),
    /// updates `REG_RDT` to the replenished descriptor index (so `RDT` always trails `rx_cur`),
    /// and advances `rx_cur`. Returns `None` if no valid packet is ready.
    pub fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        let hal = &self.hal;
        let base = self.mmio_base;
        self.rings
            .receive(buf, |rdt| Self::safe_write_tail(hal, base, REG_RDT, rdt))
    }

    pub fn read_stats(&self) -> E1000Stats {
        E1000Stats {
            gprc: self.mmio_read(REG_GPRC),
            gptc: self.mmio_read(REG_GPTC),
            mpc: self.mmio_read(REG_MPC),
            rnbc: self.mmio_read(REG_RNBC),
            crcerrs: self.mmio_read(REG_CRCERRS),
            rdh: self.mmio_read(REG_RDH),
            rdt: self.mmio_read(REG_RDT),
            tdh: self.mmio_read(REG_TDH),
            tdt: self.mmio_read(REG_TDT),
            status: self.mmio_read(REG_STATUS),
            tctl: self.mmio_read(REG_TCTL),
            txdctl: self.mmio_read(REG_TXDCTL),
            rctl: self.mmio_read(REG_RCTL),
            rxdctl: self.mmio_read(REG_RXDCTL),
        }
    }
}

impl<H: Hal> Drop for E1000Device<H> {
    fn drop(&mut self) {
        self.mmio_write(REG_RCTL, 0);
        self.mmio_write(REG_TCTL, 0);
        self.mmio_write(REG_IMC, 0xFFFF_FFFF);
        let _ = self.mmio_read(REG_ICR);
        fence(Ordering::SeqCst);

        self.rings.free(&self.hal);
    }
}

/// Returns `true` if `pci` is a supported Intel `e1000` / `e1000e` (8254x, 8256x, 82571..82574L,
/// 82577..82579, 82583V, I217, I218, I219) 1GbE controller.
/// Rejects incompatible `e100` (`0x1229`), `igb` (82575/82576/82580/I210/I211/I350), `igc` (I225/I226),
/// `ixgbe` (10GbE), `i40e` (40GbE), `ice` (100GbE), and Intel Wi-Fi adapters.
pub fn probe(pci: &PciDevice) -> bool {
    if pci.vendor_id != 0x8086 || pci.class_code != 0x02 || pci.subclass != 0x00 {
        return false;
    }
    matches!(
        pci.device_id,
        // 8254x (e1000 / QEMU e1000)
        0x1000..=0x1004
            | 0x1008..=0x101A
            | 0x101D..=0x101E
            | 0x1026..=0x1028
            | 0x1075..=0x107C
            | 0x108A
            | 0x1099
            | 0x10B5
            // 82571 / 82572 / 82573 / 82574L / 82583V (e1000e discrete PCIe & QEMU e1000e)
            | 0x105E..=0x1060
            | 0x107D..=0x107F
            | 0x108B..=0x108C
            | 0x1096
            | 0x1098
            | 0x109A
            | 0x10A4..=0x10A5
            | 0x10B9..=0x10BC
            | 0x10CB
            | 0x10D3
            | 0x10D5
            | 0x10D9..=0x10DA
            | 0x10F6
            | 0x150C
            // ICH8 / ICH9 / ICH10 / PCH / 82567 / 82577 / 82578 / 82579 LOM
            | 0x1049..=0x104D
            | 0x10BD..=0x10C5
            | 0x10CC..=0x10CE
            | 0x10DE..=0x10DF
            | 0x10E5
            | 0x10EA..=0x10EF
            | 0x10F5
            | 0x1501..=0x1503
            | 0x294C
            // I217 / I218 / I219 PCH LOM (Lynx Point through Arrow / Lunar Lake)
            | 0x0D4C..=0x0D4F
            | 0x0D53
            | 0x0D55
            | 0x0DC5..=0x0DC8
            | 0x153A..=0x153B
            | 0x1559..=0x155A
            | 0x156F..=0x1570
            | 0x15A0..=0x15A3
            | 0x15B7..=0x15B9
            | 0x15BB..=0x15BE
            | 0x15D6..=0x15D8
            | 0x15E3
            | 0x15F4
            | 0x15F9..=0x15FC
            | 0x1A1C..=0x1A1F
            | 0x550A..=0x550F
            | 0x57A0..=0x57A1
    )
}
