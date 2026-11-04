use core::sync::atomic::{Ordering, fence};

use crate::error::{Error, Result};
use crate::hal::Hal;
use crate::net::intel::{IntelRings, read_ral_rah_mac, write_mta_and_rar0};
use crate::net::resolve_mac;
use crate::pci::PciDevice;

pub use crate::net::intel::{BUFFER_SIZE, NUM_RX_DESC, NUM_TX_DESC, RxDesc, TxDesc};

const REG_CTRL: u64 = 0x0000;
const REG_STATUS: u64 = 0x0008;
const REG_CTRL_EXT: u64 = 0x0018;
const REG_EICR: u64 = 0x01580;
const REG_EIMC: u64 = 0x01528;
const REG_ICR: u64 = 0x01500;
const REG_IMC: u64 = 0x01508;
const REG_RCTL: u64 = 0x0100;
const REG_TCTL: u64 = 0x0400;
const REG_TIPG: u64 = 0x0410;
const REG_RFCTL: u64 = 0x5008;
const REG_MTA: u64 = 0x5200;
const REG_RAL0: u64 = 0x5400;
const REG_RAH0: u64 = 0x5404;
const REG_MRQC: u64 = 0x5818;

// Intel Foxville (I225/I226) Queue 0 RX registers (base 0xC000)
const REG_RDBAL0: u64 = 0xC000;
const REG_RDBAH0: u64 = 0xC004;
const REG_RDLEN0: u64 = 0xC008;
const REG_SRRCTL0: u64 = 0xC00C;
const REG_RDH0: u64 = 0xC010;
const REG_RDT0: u64 = 0xC018;
const REG_RXDCTL0: u64 = 0xC028;

// Intel Foxville (I225/I226) Queue 0 TX registers (base 0xE000)
const REG_TDBAL0: u64 = 0xE000;
const REG_TDBAH0: u64 = 0xE004;
const REG_TDLEN0: u64 = 0xE008;
const REG_TDH0: u64 = 0xE010;
const REG_TDT0: u64 = 0xE018;
const REG_TXDCTL0: u64 = 0xE028;

const CTRL_SLU: u32 = 1 << 6;
const CTRL_RST: u32 = 1 << 26;
const CTRL_DEV_RST: u32 = 1 << 29;
const CTRL_EXT_DRV_LOAD: u32 = 1 << 28;

const RFCTL_EXSTEN: u32 = 1 << 15;

const RCTL_EN: u32 = 1 << 1;
const RCTL_UPE: u32 = 1 << 3;
const RCTL_MPE: u32 = 1 << 4;
const RCTL_BAM: u32 = 1 << 15;
const RCTL_SZ_2048: u32 = 0 << 16;
const RCTL_SECRC: u32 = 1 << 26;

const SRRCTL_BSIZEPKT_2K: u32 = 2; // 2 KiB in 1 KiB units (bits 6:0)
const SRRCTL_DESCTYPE_LEGACY: u32 = 0 << 25;
const SRRCTL_DROP_EN: u32 = 1 << 31;

const RXDCTL_QUEUE_ENABLE: u32 = 1 << 25;

const TCTL_EN: u32 = 1 << 1;
const TCTL_PSP: u32 = 1 << 3;
const TCTL_CT_SHIFT: u32 = 4;
const TCTL_COLD_SHIFT: u32 = 12;

const TXDCTL_QUEUE_ENABLE: u32 = 1 << 25;

pub struct IgcDevice<H: Hal> {
    pub pci: PciDevice,
    pub mmio_base: u64,
    pub hal: H,
    mac: [u8; 6],
    rings: IntelRings,
}

impl<H: Hal> IgcDevice<H> {
    fn mmio_read(&self, offset: u64) -> u32 {
        self.hal.mmio_read32(self.mmio_base + offset)
    }

    fn mmio_write(&self, offset: u64, val: u32) {
        self.hal.mmio_write32(self.mmio_base + offset, val);
    }

    /// Initializes an Intel `igc` (I225-LM, I225-V, I225-IT, I225-K, I226-LM, I226-V, I226-IT) 2.5GbE controller:
    /// - Preserves the UEFI PXE PHY link by avoiding `CTRL.RST` / `CTRL.DEV_RST`.
    /// - Masks legacy and extended interrupts (`IMC`, `EIMC`) and sets `CTRL_EXT.DRV_LOAD`.
    /// - Reads MAC from `RAL0`/`RAH0` (`0x5400`/`0x5404`), falling back to `fallback_mac` or locally-administered default.
    /// - Clears `MRQC` (`0x5818`, disabling multi-queue RSS) and `RFCTL.EXSTEN` (`0x5008`).
    /// - Configures Queue 0 RX at `0xC000..0xC028` (`SRRCTL0 = 2 KiB | Legacy Desc`, `RXDCTL0.ENABLE`, `RCTL.EN`, then `RDT0 = NUM_RX_DESC - 1`).
    /// - Configures Queue 0 TX at `0xE000..0xE028` (`TXDCTL0.ENABLE`, `TCTL.EN | TCTL.PSP`).
    pub fn new(
        pci: PciDevice,
        mmio_base: u64,
        fallback_mac: Option<[u8; 6]>,
        hal: H,
    ) -> Result<Self> {
        if mmio_base == 0 {
            return Err(Error::msg("IGC MMIO base is 0"));
        }
        pci.enable_bus_mastering(&hal);
        hal.map_mmio(mmio_base, 0x20000);

        if hal.mmio_read32(mmio_base + REG_STATUS) == 0xFFFF_FFFF {
            return Err(Error::msg("IGC MMIO read returned 0xFFFFFFFF"));
        }

        // 1. Read MAC from RAL0/RAH0, falling back to fallback_mac or locally-administered default
        let hw_mac = read_ral_rah_mac(&hal, mmio_base, REG_RAL0, REG_RAH0);
        let mac = resolve_mac(hw_mac, fallback_mac);

        // 2. Disable all interrupts (legacy and extended MSI-X)
        hal.mmio_write32(mmio_base + REG_IMC, 0xFFFF_FFFF);
        hal.mmio_write32(mmio_base + REG_EIMC, 0xFFFF_FFFF);
        let _ = hal.mmio_read32(mmio_base + REG_ICR);
        let _ = hal.mmio_read32(mmio_base + REG_EICR);

        // 3. Stop RX and TX cleanly without asserting CTRL.RST or CTRL.DEV_RST
        hal.mmio_write32(mmio_base + REG_RCTL, 0);
        hal.mmio_write32(mmio_base + REG_TCTL, 0);
        for q in 0..4u64 {
            let off = q * 0x40;
            hal.mmio_write32(mmio_base + REG_RXDCTL0 + off, 0);
            hal.mmio_write32(mmio_base + REG_RDBAH0 + off, 0);
            hal.mmio_write32(mmio_base + REG_RDBAL0 + off, 0);
            hal.mmio_write32(mmio_base + REG_RDLEN0 + off, 0);
            hal.mmio_write32(mmio_base + REG_RDH0 + off, 0);
            hal.mmio_write32(mmio_base + REG_RDT0 + off, 0);

            hal.mmio_write32(mmio_base + REG_TXDCTL0 + off, 0);
            hal.mmio_write32(mmio_base + REG_TDBAH0 + off, 0);
            hal.mmio_write32(mmio_base + REG_TDBAL0 + off, 0);
            hal.mmio_write32(mmio_base + REG_TDLEN0 + off, 0);
            hal.mmio_write32(mmio_base + REG_TDH0 + off, 0);
            hal.mmio_write32(mmio_base + REG_TDT0 + off, 0);
        }

        // 4. Clear RFCTL.EXSTEN and MRQC
        let rfctl = hal.mmio_read32(mmio_base + REG_RFCTL);
        let new_rfctl = if rfctl == 0xFFFF_FFFF {
            0
        } else {
            rfctl & !RFCTL_EXSTEN
        };
        hal.mmio_write32(mmio_base + REG_RFCTL, new_rfctl);
        hal.mmio_write32(mmio_base + REG_MRQC, 0);

        // 5. Force link up in CTRL (without reset) and set CTRL_EXT.DRV_LOAD
        let ctrl = hal.mmio_read32(mmio_base + REG_CTRL);
        let ctrl_base = if ctrl == 0xFFFF_FFFF { 0 } else { ctrl };
        hal.mmio_write32(
            mmio_base + REG_CTRL,
            (ctrl_base & !(CTRL_RST | CTRL_DEV_RST)) | CTRL_SLU,
        );
        let ctrl_ext = hal.mmio_read32(mmio_base + REG_CTRL_EXT);
        let ctrl_ext_base = if ctrl_ext == 0xFFFF_FFFF { 0 } else { ctrl_ext };
        hal.mmio_write32(mmio_base + REG_CTRL_EXT, ctrl_ext_base | CTRL_EXT_DRV_LOAD);

        // 6. Clear MTA and program RAL0/RAH0
        write_mta_and_rar0(&hal, mmio_base, REG_MTA, REG_RAL0, REG_RAH0, mac);

        // 7. Allocate DMA rings and buffers
        let mut rings = IntelRings::alloc(&hal, "Out of DMA memory for igc")?;

        // 8. Initialize Queue 0 RX ring (0xC000..0xC028)
        hal.mmio_write32(mmio_base + REG_RDBAH0, (rings.rx_ring.phys >> 32) as u32);
        hal.mmio_write32(mmio_base + REG_RDBAL0, rings.rx_ring.phys as u32);
        hal.mmio_write32(mmio_base + REG_RDLEN0, IntelRings::RX_RING_BYTES as u32);
        hal.mmio_write32(
            mmio_base + REG_SRRCTL0,
            SRRCTL_BSIZEPKT_2K | SRRCTL_DESCTYPE_LEGACY | SRRCTL_DROP_EN,
        );
        hal.mmio_write32(mmio_base + REG_RDH0, 0);
        hal.mmio_write32(mmio_base + REG_RDT0, 0);

        let rxdctl = 16 | (4 << 8) | (1 << 16) | RXDCTL_QUEUE_ENABLE;
        hal.mmio_write32(mmio_base + REG_RXDCTL0, rxdctl);
        for _ in 0..1_000 {
            if (hal.mmio_read32(mmio_base + REG_RXDCTL0) & RXDCTL_QUEUE_ENABLE) != 0 {
                break;
            }
        }

        let rctl = RCTL_EN | RCTL_UPE | RCTL_MPE | RCTL_BAM | RCTL_SZ_2048 | RCTL_SECRC;
        hal.mmio_write32(mmio_base + REG_RCTL, rctl);
        fence(Ordering::SeqCst);
        hal.mmio_write32(mmio_base + REG_RDT0, (NUM_RX_DESC - 1) as u32);

        // 9. Initialize Queue 0 TX ring (0xE000..0xE028)
        hal.mmio_write32(mmio_base + REG_TDBAH0, (rings.tx_ring.phys >> 32) as u32);
        hal.mmio_write32(mmio_base + REG_TDBAL0, rings.tx_ring.phys as u32);
        hal.mmio_write32(mmio_base + REG_TDLEN0, IntelRings::TX_RING_BYTES as u32);
        hal.mmio_write32(mmio_base + REG_TDH0, 0);
        hal.mmio_write32(mmio_base + REG_TDT0, 0);

        hal.mmio_write32(mmio_base + REG_TIPG, 8 | (8 << 10) | (6 << 20));
        let txdctl = 0x1F | (1 << 8) | (1 << 16) | TXDCTL_QUEUE_ENABLE;
        hal.mmio_write32(mmio_base + REG_TXDCTL0, txdctl);
        for _ in 0..1_000 {
            if (hal.mmio_read32(mmio_base + REG_TXDCTL0) & TXDCTL_QUEUE_ENABLE) != 0 {
                break;
            }
        }

        let tctl = TCTL_EN | TCTL_PSP | (0x0F << TCTL_CT_SHIFT) | (0x3F << TCTL_COLD_SHIFT);
        hal.mmio_write32(mmio_base + REG_TCTL, tctl);

        let hw_tdh = rings.sync_hw_tdh(hal.mmio_read32(mmio_base + REG_TDH0));
        if hw_tdh != 0 {
            hal.mmio_write32(mmio_base + REG_TDT0, hw_tdh as u32);
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

    pub fn can_transmit(&mut self) -> bool {
        let raw_tdh = self.mmio_read(REG_TDH0);
        self.rings.can_transmit(raw_tdh)
    }

    pub fn transmit(&mut self, packet: &[u8]) -> Result<()> {
        let raw_tdh = self.mmio_read(REG_TDH0);
        let hal = &self.hal;
        let base = self.mmio_base;
        self.rings.transmit(
            packet,
            raw_tdh,
            "Packet exceeds igc BUFFER_SIZE",
            "IGC TX ring is full",
            |tdt| hal.mmio_write32(base + REG_TDT0, tdt),
        )
    }

    pub fn has_packets(&self) -> bool {
        self.rings.has_packets()
    }

    pub fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        let hal = &self.hal;
        let base = self.mmio_base;
        self.rings
            .receive(buf, |rdt| hal.mmio_write32(base + REG_RDT0, rdt))
    }
}

impl<H: Hal> Drop for IgcDevice<H> {
    fn drop(&mut self) {
        self.mmio_write(REG_RCTL, 0);
        self.mmio_write(REG_TCTL, 0);
        self.mmio_write(REG_IMC, 0xFFFF_FFFF);
        self.mmio_write(REG_EIMC, 0xFFFF_FFFF);
        fence(Ordering::SeqCst);

        self.rings.free(&self.hal);
    }
}

/// Returns `true` if `pci` is an Intel `igc` (I225 / I226 Foxville) 2.5GbE controller.
pub fn probe(pci: &PciDevice) -> bool {
    if pci.vendor_id != 0x8086 || pci.class_code != 0x02 || pci.subclass != 0x00 {
        return false;
    }
    matches!(
        pci.device_id,
        0x15F2 // I225-LM
            | 0x15F3 // I225-V
            | 0x15F8 // I225-I
            | 0x3100 // I225-K
            | 0x3101 // I225-K2
            | 0x5502 // I225-LMvP
            | 0x0D9F // I225-IT
            | 0x125B // I226-LM
            | 0x125C // I226-V
            | 0x125D // I226-IT
            | 0x125E // I226-K
            | 0x3102 // I226-K
            | 0x5503 // I226-LMvP
    )
}
