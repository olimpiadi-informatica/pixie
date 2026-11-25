use core::sync::atomic::{Ordering, fence};

use crate::error::{Error, Result};
use crate::hal::Hal;
use crate::net::intel::{IntelRings, read_ral_rah_mac, write_mta_and_rar0};
use crate::net::resolve_mac;
use crate::pci::PciDevice;

pub use crate::net::intel::{AdvRxDesc, BUFFER_SIZE, NUM_RX_DESC, NUM_TX_DESC, RxDesc, TxDesc};

const REG_CTRL: u64 = 0x0000;
const REG_STATUS: u64 = 0x0008;
const REG_CTRL_EXT: u64 = 0x0018;
const REG_MDIC: u64 = 0x0020;
const REG_RCTL: u64 = 0x0100;
const REG_TCTL: u64 = 0x0400;
const REG_TIPG: u64 = 0x0410;
const REG_EIMC: u64 = 0x01528;
const REG_EICR: u64 = 0x01580;
const REG_RXCSUM: u64 = 0x5000;
const REG_RFCTL: u64 = 0x5008;
const REG_MTA: u64 = 0x5200;
const REG_RAL0: u64 = 0x5400;
const REG_RAH0: u64 = 0x5404;
const REG_MRQC: u64 = 0x5818;

// Queue register offsets relative to each queue's RX / TX base.
const Q_RDBAL: u64 = 0x00;
const Q_RDBAH: u64 = 0x04;
const Q_RDLEN: u64 = 0x08;
const Q_SRRCTL: u64 = 0x0C;
const Q_RDH: u64 = 0x10;
const Q_RDT: u64 = 0x18;
const Q_RXDCTL: u64 = 0x28;

const Q_TDBAL: u64 = 0x00;
const Q_TDBAH: u64 = 0x04;
const Q_TDLEN: u64 = 0x08;
const Q_TDH: u64 = 0x10;
const Q_TDT: u64 = 0x18;
const Q_TXDCTL: u64 = 0x28;

const CTRL_LRST: u32 = 1 << 3;
const CTRL_SLU: u32 = 1 << 6;
const CTRL_RST: u32 = 1 << 26;
const CTRL_DEV_RST: u32 = 1 << 29;
const CTRL_VME: u32 = 1 << 30;
const CTRL_PHY_RST: u32 = 1 << 31;
const CTRL_EXT_DRV_LOAD: u32 = 1 << 28;

const STATUS_LU: u32 = 1 << 1;

const MDIC_PHY_ADDR_1: u32 = 1 << 21;
const MDIC_OP_WRITE: u32 = 1 << 26;
const MDIC_READY: u32 = 1 << 28;
const MII_BMCR_AUTONEG_RESTART: u32 = 0x1340;

const RFCTL_EXSTEN: u32 = 1 << 15;

const RCTL_EN: u32 = 1 << 1;
const RCTL_UPE: u32 = 1 << 3;
const RCTL_MPE: u32 = 1 << 4;
const RCTL_BAM: u32 = 1 << 15;
const RCTL_SZ_2048: u32 = 0 << 16;
const RCTL_SECRC: u32 = 1 << 26;

const SRRCTL_BSIZEPKT_2K: u32 = 2; // 2 KiB in 1 KiB units (bits 6:0)
const SRRCTL_DESCTYPE_ADV_ONEBUF: u32 = 1 << 25;
const SRRCTL_DROP_EN: u32 = 1 << 31;

const RXDCTL_QUEUE_ENABLE: u32 = 1 << 25;

const TCTL_EN: u32 = 1 << 1;
const TCTL_PSP: u32 = 1 << 3;
const TCTL_CT_SHIFT: u32 = 4;
const TCTL_COLD_SHIFT: u32 = 12;

const TXDCTL_QUEUE_ENABLE: u32 = 1 << 25;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgbVariant {
    I82575,
    I82576,
    I82580,
    I210,
}

#[derive(Debug, Clone, Copy)]
struct IgbConfig {
    num_queues: u64,
    icr: u64,
    imc: u64,
    legacy_intr: Option<(u64, u64)>,
}

impl IgbVariant {
    const fn config(self) -> IgbConfig {
        match self {
            Self::I82575 => IgbConfig {
                num_queues: 4,
                icr: 0x00C0,
                imc: 0x00D8,
                legacy_intr: None,
            },
            Self::I82576 => IgbConfig {
                num_queues: 16,
                icr: 0x1500,
                imc: 0x150C,
                legacy_intr: Some((0x00C0, 0x00D8)),
            },
            Self::I82580 => IgbConfig {
                num_queues: 8,
                icr: 0x1500,
                imc: 0x150C,
                legacy_intr: None,
            },
            Self::I210 => IgbConfig {
                num_queues: 4,
                icr: 0x1500,
                imc: 0x150C,
                legacy_intr: None,
            },
        }
    }

    const fn rx_q_base(self, q: u64) -> u64 {
        match self {
            Self::I82575 => 0x2800 + q * 0x100,
            Self::I82576 => {
                if q < 4 {
                    0x2800 + q * 0x100
                } else {
                    0xC000 + q * 0x40
                }
            }
            Self::I82580 | Self::I210 => 0xC000 + q * 0x40,
        }
    }

    const fn tx_q_base(self, q: u64) -> u64 {
        match self {
            Self::I82575 => 0x3800 + q * 0x100,
            Self::I82576 => {
                if q < 4 {
                    0x3800 + q * 0x100
                } else {
                    0xE000 + q * 0x40
                }
            }
            Self::I82580 | Self::I210 => 0xE000 + q * 0x40,
        }
    }
}

pub struct IgbDevice<H: Hal> {
    pub pci: PciDevice,
    pub mmio_base: u64,
    pub variant: IgbVariant,
    pub hal: H,
    mac: [u8; 6],
    rings: IntelRings,
    rx0_base: u64,
    tx0_base: u64,
}

fn wait_reg_bits<H: Hal>(
    hal: &H,
    base: u64,
    reg: u64,
    mask: u32,
    expect_set: bool,
    timeout_us: i64,
) -> bool {
    let start = hal.micros();
    let mut last = start;
    let mut stalled = 0usize;
    loop {
        let v = hal.mmio_read32(base + reg);
        if v == 0xFFFF_FFFF {
            return !expect_set;
        }
        let is_set = (v & mask) == mask;
        if is_set == expect_set {
            return true;
        }
        let now = hal.micros();
        if now.saturating_sub(start) >= timeout_us {
            return false;
        }
        if now == last {
            stalled += 1;
            if stalled >= 20_000 {
                return false;
            }
        } else {
            last = now;
            stalled = 0;
        }
        core::hint::spin_loop();
    }
}

fn mask_and_clear_interrupts<H: Hal>(hal: &H, mmio_base: u64, cfg: IgbConfig) {
    hal.mmio_write32(mmio_base + cfg.imc, 0xFFFF_FFFF);
    if let Some((_, leg_imc)) = cfg.legacy_intr {
        hal.mmio_write32(mmio_base + leg_imc, 0xFFFF_FFFF);
    }
    hal.mmio_write32(mmio_base + REG_EIMC, 0xFFFF_FFFF);
    let _ = hal.mmio_read32(mmio_base + cfg.icr);
    if let Some((leg_icr, _)) = cfg.legacy_intr {
        let _ = hal.mmio_read32(mmio_base + leg_icr);
    }
    let _ = hal.mmio_read32(mmio_base + REG_EICR);
}

impl<H: Hal> IgbDevice<H> {
    fn mmio_read(&self, offset: u64) -> u32 {
        self.hal.mmio_read32(self.mmio_base + offset)
    }

    fn mmio_write(&self, offset: u64, val: u32) {
        self.hal.mmio_write32(self.mmio_base + offset, val);
    }

    /// Initializes an Intel `igb` (82575 / 82576 / 82580 / I350 / I354 / DH89xxCC / I210 / I211)
    /// 1GbE controller, handling both UEFI PXE and dirty Linux `igb` handovers cleanly without
    /// asserting `CTRL.RST` / `CTRL.DEV_RST` / `CTRL.PHY_RST`.
    pub fn new(
        pci: PciDevice,
        mmio_base: u64,
        fallback_mac: Option<[u8; 6]>,
        hal: H,
    ) -> Result<Self> {
        if mmio_base == 0 {
            return Err(Error::msg("IGB MMIO base is 0"));
        }
        let variant = probe_variant(&pci).ok_or_else(|| Error::msg("Unsupported igb PCI ID"))?;
        let cfg = variant.config();
        let rx0_base = variant.rx_q_base(0);
        let tx0_base = variant.tx_q_base(0);

        pci.enable_bus_mastering(&hal);
        hal.map_mmio(mmio_base, 0x20000);

        if hal.mmio_read32(mmio_base + REG_STATUS) == 0xFFFF_FFFF {
            return Err(Error::msg("IGB MMIO read returned 0xFFFFFFFF"));
        }

        // 1. Read MAC from RAL0/RAH0, falling back to fallback_mac or locally-administered default
        let hw_mac = read_ral_rah_mac(&hal, mmio_base, REG_RAL0, REG_RAH0);
        let mac = resolve_mac(hw_mac, fallback_mac);

        // 2. Mask legacy + extended interrupts and clear pending causes
        mask_and_clear_interrupts(&hal, mmio_base, cfg);

        // 3. Disable global RCTL/TCTL and all hardware queues left active by UEFI or Linux `igb`
        hal.mmio_write32(mmio_base + REG_RCTL, 0);
        hal.mmio_write32(mmio_base + REG_TCTL, 0);
        for q in 0..cfg.num_queues {
            hal.mmio_write32(mmio_base + variant.rx_q_base(q) + Q_RXDCTL, 0);
            hal.mmio_write32(mmio_base + variant.tx_q_base(q) + Q_TXDCTL, 0);
        }
        for q in 0..cfg.num_queues {
            let rx_base = variant.rx_q_base(q);
            let tx_base = variant.tx_q_base(q);
            let _ = wait_reg_bits(
                &hal,
                mmio_base,
                rx_base + Q_RXDCTL,
                RXDCTL_QUEUE_ENABLE,
                false,
                20_000,
            );
            let _ = wait_reg_bits(
                &hal,
                mmio_base,
                tx_base + Q_TXDCTL,
                TXDCTL_QUEUE_ENABLE,
                false,
                20_000,
            );
            hal.mmio_write32(mmio_base + rx_base + Q_RDBAH, 0);
            hal.mmio_write32(mmio_base + rx_base + Q_RDBAL, 0);
            hal.mmio_write32(mmio_base + rx_base + Q_RDLEN, 0);
            hal.mmio_write32(mmio_base + rx_base + Q_RDH, 0);
            hal.mmio_write32(mmio_base + rx_base + Q_RDT, 0);

            hal.mmio_write32(mmio_base + tx_base + Q_TDBAH, 0);
            hal.mmio_write32(mmio_base + tx_base + Q_TDBAL, 0);
            hal.mmio_write32(mmio_base + tx_base + Q_TDLEN, 0);
            hal.mmio_write32(mmio_base + tx_base + Q_TDH, 0);
            hal.mmio_write32(mmio_base + tx_base + Q_TDT, 0);
        }

        // 4. Clear MRQC, RXCSUM, and RFCTL.EXSTEN left by Linux `igb`
        hal.mmio_write32(mmio_base + REG_MRQC, 0);
        hal.mmio_write32(mmio_base + REG_RXCSUM, 0);
        let rfctl = hal.mmio_read32(mmio_base + REG_RFCTL);
        let new_rfctl = if rfctl == 0xFFFF_FFFF {
            0
        } else {
            rfctl & !RFCTL_EXSTEN
        };
        hal.mmio_write32(mmio_base + REG_RFCTL, new_rfctl);

        // 5. Preserve PHY link (NO CTRL.RST/DEV_RST/PHY_RST); clear LRST/VME, set SLU and DRV_LOAD.
        // On cold boot (STATUS.LU == 0), restart PHY auto-negotiation via MDIC so the link comes up.
        let ctrl = hal.mmio_read32(mmio_base + REG_CTRL);
        let ctrl_base = if ctrl == 0xFFFF_FFFF { 0 } else { ctrl };
        hal.mmio_write32(
            mmio_base + REG_CTRL,
            (ctrl_base & !(CTRL_LRST | CTRL_RST | CTRL_DEV_RST | CTRL_VME | CTRL_PHY_RST))
                | CTRL_SLU,
        );
        let ctrl_ext = hal.mmio_read32(mmio_base + REG_CTRL_EXT);
        let ctrl_ext_base = if ctrl_ext == 0xFFFF_FFFF { 0 } else { ctrl_ext };
        hal.mmio_write32(mmio_base + REG_CTRL_EXT, ctrl_ext_base | CTRL_EXT_DRV_LOAD);

        if (hal.mmio_read32(mmio_base + REG_STATUS) & STATUS_LU) == 0 {
            hal.mmio_write32(
                mmio_base + REG_MDIC,
                MDIC_OP_WRITE | MDIC_PHY_ADDR_1 | MII_BMCR_AUTONEG_RESTART,
            );
            let _ = wait_reg_bits(&hal, mmio_base, REG_MDIC, MDIC_READY, true, 20_000);
            let _ = wait_reg_bits(&hal, mmio_base, REG_STATUS, STATUS_LU, true, 750_000);
        }

        // 6. Clear MTA and program RAL0/RAH0
        write_mta_and_rar0(&hal, mmio_base, REG_MTA, REG_RAL0, REG_RAH0, mac);

        // 7. Allocate DMA rings and buffers
        let mut rings = IntelRings::alloc(&hal, "Out of DMA memory for igb")?;

        // 8. Configure Queue 0 RX ring
        hal.mmio_write32(
            mmio_base + rx0_base + Q_RDBAH,
            (rings.rx_ring.phys >> 32) as u32,
        );
        hal.mmio_write32(mmio_base + rx0_base + Q_RDBAL, rings.rx_ring.phys as u32);
        hal.mmio_write32(
            mmio_base + rx0_base + Q_RDLEN,
            IntelRings::RX_RING_BYTES as u32,
        );
        hal.mmio_write32(
            mmio_base + rx0_base + Q_SRRCTL,
            SRRCTL_BSIZEPKT_2K | SRRCTL_DESCTYPE_ADV_ONEBUF | SRRCTL_DROP_EN,
        );
        hal.mmio_write32(mmio_base + rx0_base + Q_RDH, 0);
        let hw_rdh = hal.mmio_read32(mmio_base + rx0_base + Q_RDH);
        let rx_start = if (hw_rdh as usize) < NUM_RX_DESC {
            hw_rdh as usize
        } else {
            0
        };
        rings.rx_cur = rx_start;
        hal.mmio_write32(mmio_base + rx0_base + Q_RDT, rx_start as u32);

        hal.mmio_write32(mmio_base + rx0_base + Q_RXDCTL, RXDCTL_QUEUE_ENABLE);
        let _ = wait_reg_bits(
            &hal,
            mmio_base,
            rx0_base + Q_RXDCTL,
            RXDCTL_QUEUE_ENABLE,
            true,
            20_000,
        );

        let rctl = RCTL_EN | RCTL_UPE | RCTL_MPE | RCTL_BAM | RCTL_SZ_2048 | RCTL_SECRC;
        hal.mmio_write32(mmio_base + REG_RCTL, rctl);
        fence(Ordering::SeqCst);
        let rdt_init = (rx_start + NUM_RX_DESC - 1) % NUM_RX_DESC;
        hal.mmio_write32(mmio_base + rx0_base + Q_RDT, rdt_init as u32);

        // 9. Configure Queue 0 TX ring, syncing TDT0 to TDH0 BEFORE enabling TX
        hal.mmio_write32(
            mmio_base + tx0_base + Q_TDBAH,
            (rings.tx_ring.phys >> 32) as u32,
        );
        hal.mmio_write32(mmio_base + tx0_base + Q_TDBAL, rings.tx_ring.phys as u32);
        hal.mmio_write32(
            mmio_base + tx0_base + Q_TDLEN,
            IntelRings::TX_RING_BYTES as u32,
        );
        hal.mmio_write32(mmio_base + tx0_base + Q_TDH, 0);
        let pre_tdh = rings.sync_hw_tdh(hal.mmio_read32(mmio_base + tx0_base + Q_TDH));
        hal.mmio_write32(mmio_base + tx0_base + Q_TDT, pre_tdh as u32);

        hal.mmio_write32(mmio_base + REG_TIPG, 8 | (8 << 10) | (6 << 20));
        let txdctl = 0x1F | (1 << 8) | (1 << 16) | TXDCTL_QUEUE_ENABLE;
        hal.mmio_write32(mmio_base + tx0_base + Q_TXDCTL, txdctl);
        let _ = wait_reg_bits(
            &hal,
            mmio_base,
            tx0_base + Q_TXDCTL,
            TXDCTL_QUEUE_ENABLE,
            true,
            20_000,
        );

        let tctl = TCTL_EN | TCTL_PSP | (0x0F << TCTL_CT_SHIFT) | (0x3F << TCTL_COLD_SHIFT);
        hal.mmio_write32(mmio_base + REG_TCTL, tctl);

        let post_tdh = rings.sync_hw_tdh(hal.mmio_read32(mmio_base + tx0_base + Q_TDH));
        if hal.mmio_read32(mmio_base + tx0_base + Q_TDT) != post_tdh as u32 {
            hal.mmio_write32(mmio_base + tx0_base + Q_TDT, post_tdh as u32);
        }

        // Re-mask and clear interrupts after queue startup
        mask_and_clear_interrupts(&hal, mmio_base, cfg);

        Ok(Self {
            pci,
            mmio_base,
            variant,
            hal,
            mac,
            rings,
            rx0_base,
            tx0_base,
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
        self.rings.can_transmit()
    }

    pub fn transmit(&mut self, packet: &[u8]) -> Result<()> {
        let hal = &self.hal;
        let base = self.mmio_base;
        let tdt_reg = self.tx0_base + Q_TDT;
        self.rings.transmit(
            packet,
            "Invalid igb packet length",
            "IGB TX ring is full",
            |tdt| hal.mmio_write32(base + tdt_reg, tdt),
        )
    }

    pub fn has_packets(&self) -> bool {
        self.rings.has_packets_adv()
    }

    pub fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        let hal = &self.hal;
        let base = self.mmio_base;
        let rdt_reg = self.rx0_base + Q_RDT;
        self.rings
            .receive_adv(buf, |rdt| hal.mmio_write32(base + rdt_reg, rdt))
    }
}

impl<H: Hal> Drop for IgbDevice<H> {
    fn drop(&mut self) {
        let cfg = self.variant.config();
        mask_and_clear_interrupts(&self.hal, self.mmio_base, cfg);
        self.mmio_write(REG_RCTL, 0);
        self.mmio_write(REG_TCTL, 0);
        self.mmio_write(self.rx0_base + Q_RXDCTL, 0);
        self.mmio_write(self.tx0_base + Q_TXDCTL, 0);

        let rx_stopped = wait_reg_bits(
            &self.hal,
            self.mmio_base,
            self.rx0_base + Q_RXDCTL,
            RXDCTL_QUEUE_ENABLE,
            false,
            50_000,
        );
        let tx_stopped = wait_reg_bits(
            &self.hal,
            self.mmio_base,
            self.tx0_base + Q_TXDCTL,
            TXDCTL_QUEUE_ENABLE,
            false,
            50_000,
        );
        let ctrl_ext = self.mmio_read(REG_CTRL_EXT);
        if ctrl_ext != 0xFFFF_FFFF && (ctrl_ext & CTRL_EXT_DRV_LOAD) != 0 {
            self.mmio_write(REG_CTRL_EXT, ctrl_ext & !CTRL_EXT_DRV_LOAD);
        }
        let cmd = self.pci.read_u16(&self.hal, 0x04);
        if cmd != 0xFFFF && (cmd & 0x0004) != 0 {
            self.pci.write_u16(&self.hal, 0x04, cmd & !0x0004);
        }
        fence(Ordering::SeqCst);

        if rx_stopped && tx_stopped {
            self.rings.free(&self.hal);
        }
    }
}

/// Returns `Some(IgbVariant)` if `pci` is a supported Intel `igb` Gigabit Ethernet controller.
pub fn probe_variant(pci: &PciDevice) -> Option<IgbVariant> {
    if pci.vendor_id != 0x8086 || pci.class_code != 0x02 || pci.subclass != 0x00 {
        return None;
    }
    match pci.device_id {
        // 82575
        0x10A7 | 0x10A9 | 0x10D6 => Some(IgbVariant::I82575),
        // 82576
        0x10C9 | 0x10E6 | 0x10E7 | 0x10E8 | 0x150A | 0x150D | 0x1518 | 0x1526 => {
            Some(IgbVariant::I82576)
        }
        // 82580 / I350 / I354 / DH89xxCC
        0x150E..=0x1511
        | 0x1516
        | 0x1527
        | 0x1521..=0x1524
        | 0x1546
        | 0x1F40
        | 0x1F41
        | 0x1F45
        | 0x0438
        | 0x043A
        | 0x043C
        | 0x0440 => Some(IgbVariant::I82580),
        // I210 / I211
        0x1533 | 0x1534 | 0x1536 | 0x1537 | 0x1538 | 0x157B | 0x157C | 0x1539 => {
            Some(IgbVariant::I210)
        }
        _ => None,
    }
}

/// Returns `true` if `pci` is an Intel `igb` Gigabit Ethernet controller.
pub fn probe(pci: &PciDevice) -> bool {
    probe_variant(pci).is_some()
}
