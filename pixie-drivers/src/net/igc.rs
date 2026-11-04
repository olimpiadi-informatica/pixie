use core::ptr;
use core::sync::atomic::{Ordering, fence};

use crate::error::{Error, Result};
use crate::hal::{DmaBuffer, Hal, PAGE_SIZE};
use crate::pci::PciDevice;

pub const NUM_RX_DESC: usize = 512;
pub const NUM_TX_DESC: usize = 256;
pub const BUFFER_SIZE: usize = 2048;

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
const RXDCTL_SWFLUSH: u32 = 1 << 26;

const TCTL_EN: u32 = 1 << 1;
const TCTL_PSP: u32 = 1 << 3;
const TCTL_CT_SHIFT: u32 = 4;
const TCTL_COLD_SHIFT: u32 = 12;

const TXDCTL_QUEUE_ENABLE: u32 = 1 << 25;
const TXDCTL_SWFLUSH: u32 = 1 << 26;

const CMD_EOP: u8 = 1 << 0;
const CMD_IFCS: u8 = 1 << 1;
const CMD_RS: u8 = 1 << 3;

pub use crate::net::e1000::{RxDesc, TxDesc};

pub struct IgcDevice<H: Hal> {
    pub pci: PciDevice,
    pub mmio_base: u64,
    pub hal: H,
    mac: [u8; 6],
    rx_ring: DmaBuffer,
    rx_bufs: DmaBuffer,
    rx_cur: usize,
    tx_ring: DmaBuffer,
    tx_bufs: DmaBuffer,
    tx_cur: usize,
    tx_clean: usize,
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
    /// - Reads MAC from `RAL0`/`RAH0` (`0x5400`/`0x5404`), falling back to `fallback_mac`.
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

        // 1. Read MAC from RAL0/RAH0, falling back to fallback_mac
        let is_valid_mac = |m: &[u8; 6]| *m != [0; 6] && *m != [0xFF; 6] && (m[0] & 1) == 0;
        let mut mac = [0u8; 6];
        let ral = hal.mmio_read32(mmio_base + REG_RAL0);
        let rah = hal.mmio_read32(mmio_base + REG_RAH0);
        if ral != 0 && ral != 0xFFFF_FFFF {
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
        if !is_valid_mac(&mac)
            && let Some(fb) = fallback_mac
        {
            mac = fb;
        }

        // 2. Disable all interrupts (legacy and extended MSI-X)
        hal.mmio_write32(mmio_base + REG_IMC, 0xFFFF_FFFF);
        hal.mmio_write32(mmio_base + REG_EIMC, 0xFFFF_FFFF);
        let _ = hal.mmio_read32(mmio_base + REG_ICR);
        let _ = hal.mmio_read32(mmio_base + REG_EICR);

        // 3. Stop RX and TX cleanly without asserting CTRL.RST or CTRL.DEV_RST
        hal.mmio_write32(mmio_base + REG_RCTL, 0);
        hal.mmio_write32(mmio_base + REG_TCTL, 0);
        for q in 0..4u64 {
            let rx_off = q * 0x40;
            let tx_off = q * 0x40;
            hal.mmio_write32(mmio_base + REG_RXDCTL0 + rx_off, 0);
            hal.mmio_write32(mmio_base + REG_RDBAH0 + rx_off, 0);
            hal.mmio_write32(mmio_base + REG_RDBAL0 + rx_off, 0);
            hal.mmio_write32(mmio_base + REG_RDLEN0 + rx_off, 0);
            hal.mmio_write32(mmio_base + REG_RDH0 + rx_off, 0);
            hal.mmio_write32(mmio_base + REG_RDT0 + rx_off, 0);

            hal.mmio_write32(mmio_base + REG_TXDCTL0 + tx_off, 0);
            hal.mmio_write32(mmio_base + REG_TDBAH0 + tx_off, 0);
            hal.mmio_write32(mmio_base + REG_TDBAL0 + tx_off, 0);
            hal.mmio_write32(mmio_base + REG_TDLEN0 + tx_off, 0);
            hal.mmio_write32(mmio_base + REG_TDH0 + tx_off, 0);
            hal.mmio_write32(mmio_base + REG_TDT0 + tx_off, 0);
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
        for i in 0..128 {
            hal.mmio_write32(mmio_base + REG_MTA + (i * 4) as u64, 0);
        }
        let ral_val = (mac[0] as u32)
            | ((mac[1] as u32) << 8)
            | ((mac[2] as u32) << 16)
            | ((mac[3] as u32) << 24);
        let rah_val = (mac[4] as u32) | ((mac[5] as u32) << 8) | (1 << 31);
        hal.mmio_write32(mmio_base + REG_RAL0, ral_val);
        hal.mmio_write32(mmio_base + REG_RAH0, rah_val);

        // 7. Allocate DMA rings and buffers
        let rx_ring_bytes = NUM_RX_DESC * core::mem::size_of::<RxDesc>();
        let rx_ring_pages = rx_ring_bytes.div_ceil(PAGE_SIZE);
        let rx_buf_pages = (NUM_RX_DESC * BUFFER_SIZE).div_ceil(PAGE_SIZE);
        let tx_ring_bytes = NUM_TX_DESC * core::mem::size_of::<TxDesc>();
        let tx_ring_pages = tx_ring_bytes.div_ceil(PAGE_SIZE);
        let tx_buf_pages = (NUM_TX_DESC * BUFFER_SIZE).div_ceil(PAGE_SIZE);

        let rx_ring = hal
            .alloc_dma(rx_ring_pages)
            .ok_or_else(|| Error::msg("Out of DMA memory for igc RX ring"))?;
        let Some(rx_bufs) = hal.alloc_dma(rx_buf_pages) else {
            hal.free_dma(rx_ring);
            return Err(Error::msg("Out of DMA memory for igc RX buffers"));
        };
        let Some(tx_ring) = hal.alloc_dma(tx_ring_pages) else {
            hal.free_dma(rx_ring);
            hal.free_dma(rx_bufs);
            return Err(Error::msg("Out of DMA memory for igc TX ring"));
        };
        let Some(tx_bufs) = hal.alloc_dma(tx_buf_pages) else {
            hal.free_dma(rx_ring);
            hal.free_dma(rx_bufs);
            hal.free_dma(tx_ring);
            return Err(Error::msg("Out of DMA memory for igc TX buffers"));
        };

        // 8. Initialize Queue 0 RX ring (0xC000..0xC028)
        let rx_descs_ptr = rx_ring.virt as *mut RxDesc;
        for i in 0..NUM_RX_DESC {
            let desc = RxDesc {
                buffer_addr: rx_bufs.phys + (i * BUFFER_SIZE) as u64,
                length: 0,
                checksum: 0,
                status: 0,
                errors: 0,
                special: 0,
            };
            unsafe {
                ptr::write_volatile(rx_descs_ptr.add(i), desc);
            }
        }
        fence(Ordering::SeqCst);

        hal.mmio_write32(mmio_base + REG_RDBAH0, (rx_ring.phys >> 32) as u32);
        hal.mmio_write32(mmio_base + REG_RDBAL0, rx_ring.phys as u32);
        hal.mmio_write32(mmio_base + REG_RDLEN0, rx_ring_bytes as u32);
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
        let tx_descs_ptr = tx_ring.virt as *mut TxDesc;
        for i in 0..NUM_TX_DESC {
            let desc = TxDesc {
                buffer_addr: tx_bufs.phys + (i * BUFFER_SIZE) as u64,
                length: 0,
                cso: 0,
                cmd: 0,
                status: 1, // DD=1: available
                css: 0,
                special: 0,
            };
            unsafe {
                ptr::write_volatile(tx_descs_ptr.add(i), desc);
            }
        }
        fence(Ordering::SeqCst);

        hal.mmio_write32(mmio_base + REG_TDBAH0, (tx_ring.phys >> 32) as u32);
        hal.mmio_write32(mmio_base + REG_TDBAL0, tx_ring.phys as u32);
        hal.mmio_write32(mmio_base + REG_TDLEN0, tx_ring_bytes as u32);
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

        let hw_tdh = (hal.mmio_read32(mmio_base + REG_TDH0) as usize) % NUM_TX_DESC;
        if hw_tdh != 0 {
            hal.mmio_write32(mmio_base + REG_TDT0, hw_tdh as u32);
        }

        Ok(Self {
            pci,
            mmio_base,
            hal,
            mac,
            rx_ring,
            rx_bufs,
            rx_cur: 0,
            tx_ring,
            tx_bufs,
            tx_cur: hw_tdh,
            tx_clean: hw_tdh,
        })
    }

    pub fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    pub fn is_link_up(&self) -> bool {
        (self.mmio_read(REG_STATUS) & (1 << 1)) != 0
    }

    fn clean_tx(&mut self) {
        let tx_descs = self.tx_ring.virt as *mut TxDesc;
        while self.tx_clean != self.tx_cur {
            let status = unsafe { ptr::read_volatile(&(*tx_descs.add(self.tx_clean)).status) };
            if (status & 1) != 0 {
                self.tx_clean = (self.tx_clean + 1) % NUM_TX_DESC;
            } else {
                break;
            }
        }

        if self.tx_clean != self.tx_cur {
            let tdh = (self.mmio_read(REG_TDH0) as usize) % NUM_TX_DESC;
            let advanced = if self.tx_cur >= self.tx_clean {
                tdh > self.tx_clean && tdh <= self.tx_cur
            } else {
                tdh > self.tx_clean || tdh <= self.tx_cur
            };
            if advanced {
                while self.tx_clean != tdh {
                    unsafe {
                        ptr::write_volatile(&mut (*tx_descs.add(self.tx_clean)).status, 1);
                    }
                    self.tx_clean = (self.tx_clean + 1) % NUM_TX_DESC;
                }
            }
        }
    }

    pub fn can_transmit(&mut self) -> bool {
        self.clean_tx();
        let tx_descs = self.tx_ring.virt as *const TxDesc;
        let cur_status = unsafe { ptr::read_volatile(&(*tx_descs.add(self.tx_cur)).status) };
        (self.tx_cur + 1) % NUM_TX_DESC != self.tx_clean && (cur_status & 1) != 0
    }

    pub fn transmit(&mut self, packet: &[u8]) -> Result<()> {
        if packet.len() > BUFFER_SIZE {
            return Err(Error::msg("Packet exceeds igc BUFFER_SIZE"));
        }
        if !self.can_transmit() {
            return Err(Error::msg("IGC TX ring is full"));
        }

        let idx = self.tx_cur;
        let buf_phys = self.tx_bufs.phys + (idx * BUFFER_SIZE) as u64;
        let buf_ptr = unsafe { self.tx_bufs.virt.add(idx * BUFFER_SIZE) };
        let desc_ptr = unsafe { (self.tx_ring.virt as *mut TxDesc).add(idx) };

        unsafe {
            ptr::copy_nonoverlapping(packet.as_ptr(), buf_ptr, packet.len());
            ptr::write_volatile(
                desc_ptr,
                TxDesc {
                    buffer_addr: buf_phys,
                    length: packet.len() as u16,
                    cso: 0,
                    cmd: CMD_EOP | CMD_IFCS | CMD_RS,
                    status: 0,
                    css: 0,
                    special: 0,
                },
            );
        }
        fence(Ordering::SeqCst);

        self.tx_cur = (idx + 1) % NUM_TX_DESC;
        self.mmio_write(REG_TDT0, self.tx_cur as u32);
        let _ = TXDCTL_SWFLUSH;
        let _ = RXDCTL_SWFLUSH;
        Ok(())
    }

    pub fn has_packets(&self) -> bool {
        let rx_descs = self.rx_ring.virt as *const RxDesc;
        let status = unsafe { ptr::read_volatile(&(*rx_descs.add(self.rx_cur)).status) };
        (status & 1) != 0
    }

    pub fn receive(&mut self, buf: &mut [u8]) -> Option<usize> {
        const RX_FATAL_ERRORS: u8 = 0x83;
        let rx_descs = self.rx_ring.virt as *mut RxDesc;

        for _ in 0..NUM_RX_DESC {
            let idx = self.rx_cur;
            let desc_ptr = unsafe { rx_descs.add(idx) };
            let desc = unsafe { ptr::read_volatile(desc_ptr) };
            if (desc.status & 1) == 0 {
                return None;
            }

            fence(Ordering::SeqCst);
            let buf_phys = self.rx_bufs.phys + (idx * BUFFER_SIZE) as u64;

            if (desc.errors & RX_FATAL_ERRORS) != 0 || (desc.status & 2) == 0 {
                unsafe {
                    ptr::write_volatile(
                        desc_ptr,
                        RxDesc {
                            buffer_addr: buf_phys,
                            length: 0,
                            checksum: 0,
                            status: 0,
                            errors: 0,
                            special: 0,
                        },
                    );
                }
                fence(Ordering::SeqCst);
                self.rx_cur = (idx + 1) % NUM_RX_DESC;
                self.mmio_write(REG_RDT0, idx as u32);
                continue;
            }

            let copy_len = (desc.length as usize).min(buf.len());
            let pkt_ptr = unsafe { self.rx_bufs.virt.add(idx * BUFFER_SIZE) };
            unsafe {
                ptr::copy_nonoverlapping(pkt_ptr, buf.as_mut_ptr(), copy_len);
                ptr::write_volatile(
                    desc_ptr,
                    RxDesc {
                        buffer_addr: buf_phys,
                        length: 0,
                        checksum: 0,
                        status: 0,
                        errors: 0,
                        special: 0,
                    },
                );
            }
            fence(Ordering::SeqCst);

            self.rx_cur = (idx + 1) % NUM_RX_DESC;
            self.mmio_write(REG_RDT0, idx as u32);
            return Some(copy_len);
        }
        None
    }
}

impl<H: Hal> Drop for IgcDevice<H> {
    fn drop(&mut self) {
        self.mmio_write(REG_RCTL, 0);
        self.mmio_write(REG_TCTL, 0);
        self.mmio_write(REG_IMC, 0xFFFF_FFFF);
        self.mmio_write(REG_EIMC, 0xFFFF_FFFF);

        self.hal.free_dma(self.rx_ring);
        self.hal.free_dma(self.rx_bufs);
        self.hal.free_dma(self.tx_ring);
        self.hal.free_dma(self.tx_bufs);
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
