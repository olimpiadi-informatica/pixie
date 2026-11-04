use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::time::Duration;

use pixie_drivers::disk::ahci::AhciDisk;
use pixie_drivers::hal::{DmaBuffer, Hal, PAGE_SIZE};
use pixie_drivers::pci::PciDevice;

fn block_on<F: Future>(mut fut: F) -> F::Output {
    fn raw_waker() -> RawWaker {
        fn no_op(_: *const ()) {}
        fn clone(_: *const ()) -> RawWaker {
            raw_waker()
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, no_op, no_op, no_op);
        RawWaker::new(core::ptr::null(), &VTABLE)
    }

    let waker = unsafe { Waker::from_raw(raw_waker()) };
    let mut cx = Context::from_waker(&waker);
    let mut fut = unsafe { Pin::new_unchecked(&mut fut) };

    for _ in 0..1_000_000 {
        if let Poll::Ready(output) = fut.as_mut().poll(&mut cx) {
            return output;
        }
    }
    panic!("Future did not resolve within 1,000,000 poll iterations");
}

const ABAR: u64 = 0xFEB2_0000;
const DMA_PHYS_BASE: u64 = 0x2000_0000;
const DMA_POOL_PAGES: usize = 4096; // 16 MiB
const DMA_POOL_SIZE: usize = DMA_POOL_PAGES * PAGE_SIZE;

const PORT0_BASE: u64 = 0x100;
const PX_CMD_ST: u32 = 1 << 0;
const PX_CMD_FRE: u32 = 1 << 4;
const PX_CMD_FR: u32 = 1 << 14;
const PX_CMD_CR: u32 = 1 << 15;

struct AhciSimState {
    dma_pool: Box<[u8]>,
    next_dma_page: usize,
    disk_storage: Vec<u8>,
    num_sectors: u64,
    sector_size: u32,

    // HBA registers
    cap: u32,
    ghc: u32,
    is: u32,
    pi: u32,
    vs: u32,
    cap2: u32,
    bohc: u32,
    bohc_handoff_done: bool,

    // Port 0 registers
    px_clb: u64,
    px_fb: u64,
    px_is: u32,
    px_ie: u32,
    px_cmd: u32,
    px_tfd: u32,
    px_sig: u32,
    px_ssts: u32,
    px_sctl: u32,
    px_serr: u32,
    px_sact: u32,
    px_ci: u32,

    // Quirk tracking
    comreset_count: usize,
    comreset_pending_fre: bool,
    fre_enabled_for_comreset: bool,
    max_prdtl_seen: usize,
    total_commands_issued: usize,
    flush_count: usize,
    time_us: i64,

    // PCI config space
    pci_addr_latch: u32,
    pci_command_reg: u16,
}

impl AhciSimState {
    fn new(num_sectors: u64, initial_ssts: u32, initial_tfd: u32) -> Self {
        let sector_size = 512u32;
        Self {
            dma_pool: vec![0u8; DMA_POOL_SIZE].into_boxed_slice(),
            next_dma_page: 0,
            disk_storage: vec![0u8; (num_sectors as usize) * (sector_size as usize)],
            num_sectors,
            sector_size,

            cap: (1 << 31) | (31 << 8) | 1, // S64A | 32 slots | 2 ports
            ghc: 0,
            is: 0,
            pi: 0x0000_0001, // Port 0 implemented
            vs: 0x0001_0301,
            cap2: 0x0000_0001, // BOH supported
            bohc: 0x0000_0001, // BOS = 1 (BIOS owned)
            bohc_handoff_done: false,

            px_clb: 0,
            px_fb: 0,
            px_is: 0,
            px_ie: 0,
            px_cmd: 0,
            px_tfd: initial_tfd,
            px_sig: 0x0000_0101, // SATA disk
            px_ssts: initial_ssts,
            px_sctl: 0,
            px_serr: 0,
            px_sact: 0,
            px_ci: 0,

            comreset_count: 0,
            comreset_pending_fre: false,
            fre_enabled_for_comreset: false,
            max_prdtl_seen: 0,
            total_commands_issued: 0,
            flush_count: 0,
            time_us: 1_000,

            pci_addr_latch: 0,
            pci_command_reg: 0,
        }
    }

    fn dma_slice(&self, phys: u64, len: usize) -> &[u8] {
        assert!(phys >= DMA_PHYS_BASE, "DMA phys {:#x} below base", phys);
        let off = (phys - DMA_PHYS_BASE) as usize;
        assert!(
            off + len <= self.dma_pool.len(),
            "DMA phys range out of bounds"
        );
        &self.dma_pool[off..off + len]
    }

    fn dma_slice_mut(&mut self, phys: u64, len: usize) -> &mut [u8] {
        assert!(phys >= DMA_PHYS_BASE, "DMA phys {:#x} below base", phys);
        let off = (phys - DMA_PHYS_BASE) as usize;
        assert!(
            off + len <= self.dma_pool.len(),
            "DMA phys range out of bounds"
        );
        &mut self.dma_pool[off..off + len]
    }

    fn build_identify_block(&self) -> [u8; 512] {
        let mut id = [0u8; 512];
        // Word 0: general config (non-ATAPI)
        id[0..2].copy_from_slice(&0x0040u16.to_le_bytes());
        // Serial (words 10..19)
        for (i, ch) in b"PIXIE-AHCI-SIM-00001".chunks(2).enumerate() {
            id[20 + i * 2] = ch[1];
            id[20 + i * 2 + 1] = ch[0];
        }
        // Model (words 27..46)
        let model = b"PIXIE VIRTUAL SATA DISK                 ";
        for (i, ch) in model.chunks(2).enumerate() {
            id[54 + i * 2] = ch[1];
            id[54 + i * 2 + 1] = ch[0];
        }
        // Word 47: READ/WRITE MULTIPLE support
        id[94..96].copy_from_slice(&0x8001u16.to_le_bytes());
        // Word 49: Capabilities (LBA supported = bit 9, DMA supported = bit 8)
        id[98..100].copy_from_slice(&0x0300u16.to_le_bytes());
        // Words 60..61: LBA28 total sectors
        let lba28 = self.num_sectors.min(0x0FFF_FFFF) as u32;
        id[120..124].copy_from_slice(&lba28.to_le_bytes());
        // Word 83: Command set supported (bit 10 = LBA48 supported, bit 14 = 1)
        id[166..168].copy_from_slice(&((1u16 << 10) | (1u16 << 14)).to_le_bytes());
        // Word 86: Command set enabled (bit 10 = LBA48 enabled)
        id[172..174].copy_from_slice(&(1u16 << 10).to_le_bytes());
        // Words 100..103: LBA48 total sectors (u64)
        id[200..208].copy_from_slice(&self.num_sectors.to_le_bytes());
        // Word 106: Physical / logical sector size (0x4000 = 512B logical sectors)
        id[212..214].copy_from_slice(&0x4000u16.to_le_bytes());
        id
    }

    fn execute_slot(&mut self, slot: usize) {
        assert!(
            (self.px_cmd & PX_CMD_ST) != 0,
            "Command issued while PxCMD.ST == 0"
        );
        assert!(
            (self.px_cmd & PX_CMD_FRE) != 0,
            "Command issued while PxCMD.FRE == 0"
        );
        assert_ne!(self.px_clb, 0, "PxCLB not programmed");
        assert_ne!(self.px_fb, 0, "PxFB not programmed");

        let hdr_phys = self.px_clb + (slot * 32) as u64;
        let mut hdr = [0u8; 32];
        hdr.copy_from_slice(self.dma_slice(hdr_phys, 32));

        let dw0 = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
        let cfl = dw0 & 0x1F;
        assert_eq!(
            cfl, 5,
            "Expected 5-DWORD (20-byte) H2D Register FIS, got CFL={}",
            cfl
        );
        let is_write = (dw0 & (1 << 6)) != 0;
        let prdtl = ((dw0 >> 16) & 0xFFFF) as usize;
        let ctba = u64::from_le_bytes(hdr[8..16].try_into().unwrap());
        assert_eq!(
            ctba & 0x7F,
            0,
            "Command Table Base Address {:#x} must be 128-byte aligned",
            ctba
        );

        self.max_prdtl_seen = self.max_prdtl_seen.max(prdtl);
        self.total_commands_issued += 1;

        let mut cfis = [0u8; 20];
        cfis.copy_from_slice(self.dma_slice(ctba, 20));
        assert_eq!(cfis[0], 0x27, "Expected FIS Type 0x27 (Register H2D)");
        assert_ne!(cfis[1] & 0x80, 0, "Expected C bit (bit 7) set in H2D FIS");
        let cmd = cfis[2];

        // Collect PRDT entries (starting at ctba + 0x80)
        let mut prdts = Vec::with_capacity(prdtl);
        for i in 0..prdtl {
            let entry_phys = ctba + 0x80 + (i * 16) as u64;
            let mut entry = [0u8; 16];
            entry.copy_from_slice(self.dma_slice(entry_phys, 16));
            let dba = u64::from_le_bytes(entry[0..8].try_into().unwrap());
            let dbc =
                (u32::from_le_bytes(entry[12..16].try_into().unwrap()) & 0x003F_FFFF) as usize + 1;
            prdts.push((dba, dbc));
        }

        let mut transferred = 0usize;

        match cmd {
            // ATA_CMD_IDENTIFY (0xEC)
            0xEC => {
                assert!(
                    !is_write,
                    "IDENTIFY must not set Write bit in Command Header"
                );
                let id_block = self.build_identify_block();
                let mut rem = &id_block[..];
                for (dba, dbc) in prdts {
                    let take = rem.len().min(dbc);
                    self.dma_slice_mut(dba, take).copy_from_slice(&rem[..take]);
                    rem = &rem[take..];
                    transferred += take;
                    if rem.is_empty() {
                        break;
                    }
                }
            }
            // ATA_CMD_READ_DMA_EXT (0x25) or ATA_CMD_READ_DMA (0xC8)
            0x25 | 0xC8 => {
                assert!(
                    !is_write,
                    "READ DMA must not set Write bit in Command Header"
                );
                let (lba, sectors) = if cmd == 0x25 {
                    let lba = (cfis[4] as u64)
                        | ((cfis[5] as u64) << 8)
                        | ((cfis[6] as u64) << 16)
                        | ((cfis[8] as u64) << 24)
                        | ((cfis[9] as u64) << 32)
                        | ((cfis[10] as u64) << 40);
                    let cnt = (cfis[12] as usize) | ((cfis[13] as usize) << 8);
                    (lba, if cnt == 0 { 65536 } else { cnt })
                } else {
                    let lba = (cfis[4] as u64)
                        | ((cfis[5] as u64) << 8)
                        | ((cfis[6] as u64) << 16)
                        | (((cfis[7] & 0x0F) as u64) << 24);
                    let cnt = cfis[12] as usize;
                    (lba, if cnt == 0 { 256 } else { cnt })
                };
                let total_bytes = sectors * (self.sector_size as usize);
                let mut disk_off = (lba as usize) * (self.sector_size as usize);
                let mut left = total_bytes;
                for (dba, dbc) in prdts {
                    let chunk = left.min(dbc);
                    let data = self.disk_storage[disk_off..disk_off + chunk].to_vec();
                    self.dma_slice_mut(dba, chunk).copy_from_slice(&data);
                    disk_off += chunk;
                    left -= chunk;
                    transferred += chunk;
                    if left == 0 {
                        break;
                    }
                }
                assert_eq!(
                    left, 0,
                    "PRDT buffer total shorter than requested READ sector count"
                );
            }
            // ATA_CMD_WRITE_DMA_EXT (0x35) or ATA_CMD_WRITE_DMA (0xCA)
            0x35 | 0xCA => {
                assert!(
                    is_write,
                    "WRITE DMA must set Write bit (bit 6) in Command Header"
                );
                let (lba, sectors) = if cmd == 0x35 {
                    let lba = (cfis[4] as u64)
                        | ((cfis[5] as u64) << 8)
                        | ((cfis[6] as u64) << 16)
                        | ((cfis[8] as u64) << 24)
                        | ((cfis[9] as u64) << 32)
                        | ((cfis[10] as u64) << 40);
                    let cnt = (cfis[12] as usize) | ((cfis[13] as usize) << 8);
                    (lba, if cnt == 0 { 65536 } else { cnt })
                } else {
                    let lba = (cfis[4] as u64)
                        | ((cfis[5] as u64) << 8)
                        | ((cfis[6] as u64) << 16)
                        | (((cfis[7] & 0x0F) as u64) << 24);
                    let cnt = cfis[12] as usize;
                    (lba, if cnt == 0 { 256 } else { cnt })
                };
                let total_bytes = sectors * (self.sector_size as usize);
                let mut disk_off = (lba as usize) * (self.sector_size as usize);
                let mut left = total_bytes;
                for (dba, dbc) in prdts {
                    let chunk = left.min(dbc);
                    let mut tmp = vec![0u8; chunk];
                    tmp.copy_from_slice(self.dma_slice(dba, chunk));
                    self.disk_storage[disk_off..disk_off + chunk].copy_from_slice(&tmp);
                    disk_off += chunk;
                    left -= chunk;
                    transferred += chunk;
                    if left == 0 {
                        break;
                    }
                }
                assert_eq!(
                    left, 0,
                    "PRDT buffer total shorter than requested WRITE sector count"
                );
            }
            // FLUSH CACHE EXT (0xEA) or FLUSH CACHE (0xE7)
            0xEA | 0xE7 => {
                self.flush_count += 1;
            }
            other => panic!("Unsupported ATA command {:#x}", other),
        }

        // Write PRDBC back to Command Header
        let hdr_mut = self.dma_slice_mut(hdr_phys, 32);
        hdr_mut[4..8].copy_from_slice(&(transferred as u32).to_le_bytes());

        // Clear command slot in PxCI and signal D2H completion in PxIS
        self.px_ci &= !(1 << slot);
        self.px_is |= 1;
        self.px_tfd = 0x50;
    }

    fn mmio_read32(&self, addr: u64) -> u32 {
        let off = addr.checked_sub(ABAR).expect("MMIO read below ABAR");
        match off {
            0x00 => self.cap,
            0x04 => self.ghc,
            0x08 => self.is,
            0x0C => self.pi,
            0x10 => self.vs,
            0x24 => self.cap2,
            0x28 => self.bohc,
            // Port 0 registers
            0x100 => (self.px_clb & 0xFFFF_FFFF) as u32,
            0x104 => (self.px_clb >> 32) as u32,
            0x108 => (self.px_fb & 0xFFFF_FFFF) as u32,
            0x10C => (self.px_fb >> 32) as u32,
            0x110 => self.px_is,
            0x114 => self.px_ie,
            0x118 => self.px_cmd,
            0x120 => self.px_tfd,
            0x124 => self.px_sig,
            0x128 => self.px_ssts,
            0x12C => self.px_sctl,
            0x130 => self.px_serr,
            0x134 => self.px_sact,
            0x138 => self.px_ci,
            // Other ports (unimplemented / no device)
            _ => 0,
        }
    }

    fn mmio_write32(&mut self, addr: u64, val: u32) {
        let off = addr.checked_sub(ABAR).expect("MMIO write below ABAR");
        match off {
            0x04 => {
                // GHC: if HR (bit 0) is written, clear HR immediately
                self.ghc = val & !1;
            }
            0x08 => {
                self.is &= !val; // RW1C
            }
            0x28 => {
                // BOHC: setting OOS (bit 1) clears BOS (bit 0) and BB (bit 4)
                self.bohc = val;
                if (val & (1 << 1)) != 0 {
                    self.bohc &= !((1 << 0) | (1 << 4));
                    self.bohc_handoff_done = true;
                }
            }
            // Port 0 registers (base 0x100)
            p if (PORT0_BASE..PORT0_BASE + 0x80).contains(&p) => {
                let reg = p - PORT0_BASE;
                match reg {
                    0x00 => self.px_clb = (self.px_clb & 0xFFFF_FFFF_0000_0000) | (val as u64),
                    0x04 => {
                        self.px_clb = (self.px_clb & 0x0000_0000_FFFF_FFFF) | ((val as u64) << 32)
                    }
                    0x08 => self.px_fb = (self.px_fb & 0xFFFF_FFFF_0000_0000) | (val as u64),
                    0x0C => {
                        self.px_fb = (self.px_fb & 0x0000_0000_FFFF_FFFF) | ((val as u64) << 32)
                    }
                    0x10 => self.px_is &= !val, // RW1C
                    0x14 => self.px_ie = val,
                    0x18 => {
                        let mut cmd = val & !(PX_CMD_FR | PX_CMD_CR);
                        if (cmd & PX_CMD_FRE) != 0 {
                            cmd |= PX_CMD_FR;
                            // Quirk 2: If COMRESET occurred and was waiting for FRE=1 to receive
                            // the initial D2H Register FIS, clear BSY now!
                            if self.comreset_pending_fre {
                                self.comreset_pending_fre = false;
                                self.fre_enabled_for_comreset = true;
                                self.px_tfd = 0x50;
                            }
                        }
                        if (cmd & PX_CMD_ST) != 0 {
                            cmd |= PX_CMD_CR;
                        }
                        self.px_cmd = cmd;
                    }
                    0x2C => {
                        let det = val & 0xF;
                        if det == 1 {
                            // COMRESET asserted!
                            self.comreset_count += 1;
                            self.px_tfd = 0x80; // BSY = 1 during reset
                            self.px_ssts = 0;
                            self.comreset_pending_fre = true;
                        } else if det == 0 && (self.px_sctl & 0xF) == 1 {
                            // COMRESET deasserted -> PHY link re-establishes (DET = 3)
                            self.px_ssts = 0x0000_0133;
                            self.px_sig = 0x0000_0101;
                            // Quirk 2 enforcement: ONLY clear PxTFD.BSY if PxCMD.FRE is 1!
                            if (self.px_cmd & PX_CMD_FRE) != 0 {
                                self.comreset_pending_fre = false;
                                self.fre_enabled_for_comreset = true;
                                self.px_tfd = 0x50;
                            } else {
                                self.px_tfd = 0x80; // stays BSY until FRE=1!
                            }
                        }
                        self.px_sctl = val;
                    }
                    0x30 => self.px_serr &= !val, // RW1C
                    0x34 => self.px_sact |= val,
                    0x38 => {
                        self.px_ci |= val;
                        for slot in 0..32 {
                            if (self.px_ci & (1 << slot)) != 0 {
                                self.execute_slot(slot);
                            }
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

#[derive(Clone)]
struct MockAhciHal {
    state: Arc<Mutex<AhciSimState>>,
}

impl MockAhciHal {
    fn new(num_sectors: u64, initial_ssts: u32, initial_tfd: u32) -> Self {
        Self {
            state: Arc::new(Mutex::new(AhciSimState::new(
                num_sectors,
                initial_ssts,
                initial_tfd,
            ))),
        }
    }
}

impl Hal for MockAhciHal {
    fn mmio_read8(&self, addr: u64) -> u8 {
        let aligned = addr & !3;
        let shift = ((addr & 3) * 8) as u32;
        ((self.mmio_read32(aligned) >> shift) & 0xFF) as u8
    }

    fn mmio_write8(&self, _addr: u64, _val: u8) {}

    fn mmio_read16(&self, addr: u64) -> u16 {
        let aligned = addr & !3;
        let shift = ((addr & 2) * 8) as u32;
        ((self.mmio_read32(aligned) >> shift) & 0xFFFF) as u16
    }

    fn mmio_write16(&self, _addr: u64, _val: u16) {}

    fn mmio_read32(&self, addr: u64) -> u32 {
        self.state.lock().unwrap().mmio_read32(addr)
    }

    fn mmio_write32(&self, addr: u64, val: u32) {
        self.state.lock().unwrap().mmio_write32(addr, val);
    }

    fn mmio_read64(&self, addr: u64) -> u64 {
        let st = self.state.lock().unwrap();
        let lo = st.mmio_read32(addr) as u64;
        let hi = st.mmio_read32(addr + 4) as u64;
        lo | (hi << 32)
    }

    fn mmio_write64(&self, addr: u64, val: u64) {
        let mut st = self.state.lock().unwrap();
        st.mmio_write32(addr, (val & 0xFFFF_FFFF) as u32);
        st.mmio_write32(addr + 4, (val >> 32) as u32);
    }

    fn io_read8(&self, _port: u16) -> u8 {
        0
    }
    fn io_write8(&self, _port: u16, _val: u8) {}
    fn io_read16(&self, _port: u16) -> u16 {
        0
    }
    fn io_write16(&self, _port: u16, _val: u16) {}

    fn io_read32(&self, port: u16) -> u32 {
        let st = self.state.lock().unwrap();
        if port == 0xCFC && (st.pci_addr_latch & 0xFC) == 0x04 {
            return st.pci_command_reg as u32;
        }
        0
    }

    fn io_write32(&self, port: u16, val: u32) {
        let mut st = self.state.lock().unwrap();
        if port == 0xCF8 {
            st.pci_addr_latch = val;
        } else if port == 0xCFC && (st.pci_addr_latch & 0xFC) == 0x04 {
            st.pci_command_reg = (val & 0xFFFF) as u16;
        }
    }

    fn alloc_dma(&self, pages: usize) -> Option<DmaBuffer> {
        let mut st = self.state.lock().unwrap();
        if st.next_dma_page + pages > DMA_POOL_PAGES {
            return None;
        }
        let offset = st.next_dma_page * PAGE_SIZE;
        let size = pages * PAGE_SIZE;
        st.next_dma_page += pages;
        st.dma_pool[offset..offset + size].fill(0);
        let virt = unsafe { st.dma_pool.as_mut_ptr().add(offset) };
        let phys = DMA_PHYS_BASE + offset as u64;
        Some(DmaBuffer { virt, phys, size })
    }

    fn free_dma(&self, _buf: DmaBuffer) {}

    fn phys_to_virt(&self, phys: u64) -> *mut u8 {
        let mut st = self.state.lock().unwrap();
        let offset = (phys - DMA_PHYS_BASE) as usize;
        unsafe { st.dma_pool.as_mut_ptr().add(offset) }
    }

    fn micros(&self) -> i64 {
        let mut st = self.state.lock().unwrap();
        st.time_us += 10;
        st.time_us
    }

    async fn yield_now(&self) {
        let mut st = self.state.lock().unwrap();
        st.time_us += 50;
    }

    async fn sleep(&self, duration: Duration) {
        let mut st = self.state.lock().unwrap();
        st.time_us += duration.as_micros() as i64;
    }
}

fn dummy_ahci_pci() -> PciDevice {
    PciDevice {
        bus: 0,
        dev: 31,
        func: 2,
        vendor_id: 0x8086,
        device_id: 0x2922,
        class_code: 0x01,
        subclass: 0x06,
        prog_if: 0x01,
    }
}

#[test]
fn test_ahci_quirk1_bios_initialized_active_link_preserved_without_comreset() {
    // Port 0 starts with active SATA link (PxSSTS.DET == 3, IPM == 1 -> 0x133)
    // and clean Task File (PxTFD == 0x50: BSY=0, DRQ=0, DRDY=1)
    let hal = MockAhciHal::new(4096, 0x0000_0133, 0x50);

    let ports = AhciDisk::probe_sata_ports(ABAR, &hal);
    assert_eq!(ports, vec![0]);

    let disk = block_on(AhciDisk::open(dummy_ahci_pci(), ABAR, 0, hal.clone()))
        .expect("AhciDisk::open should succeed on clean active port");

    assert_eq!(disk.block_size(), 512);
    assert_eq!(disk.num_blocks(), 4096);
    assert_eq!(disk.size(), 4096 * 512);
    assert!(disk.is_lba48());

    let st = hal.state.lock().unwrap();
    assert!(
        st.bohc_handoff_done,
        "Expected BIOS/OS handoff (BOHC.OOS) to be performed"
    );
    assert_eq!(
        st.comreset_count, 0,
        "AhciDisk::open MUST NOT issue link-disrupting COMRESET when PxSSTS.DET==3 and BSY==0 && DRQ==0"
    );
}

#[test]
fn test_ahci_quirk2_stuck_port_comreset_requires_fre_enabled() {
    // Port 0 starts with stuck BSY=1 (PxTFD = 0x80) so COMRESET is required.
    // The simulator leaves PxTFD.BSY = 1 forever after COMRESET unless PxCMD.FRE = 1 is enabled!
    let hal = MockAhciHal::new(4096, 0x0000_0133, 0x80);

    let mut disk = block_on(AhciDisk::open(dummy_ahci_pci(), ABAR, 0, hal.clone()))
        .expect("AhciDisk::open should recover stuck port via COMRESET with PxCMD.FRE=1");

    {
        let st = hal.state.lock().unwrap();
        assert!(
            st.comreset_count >= 1,
            "Expected COMRESET to be issued when port starts with BSY=1"
        );
        assert!(
            st.fre_enabled_for_comreset,
            "Expected PxCMD.FRE=1 to be enabled so initial D2H FIS clears PxTFD.BSY after COMRESET"
        );
    }

    // Verify I/O works after recovery
    let pattern = [0x5Cu8; 4096];
    block_on(disk.write(0, &pattern)).expect("Write after COMRESET should succeed");
    let mut readback = [0u8; 4096];
    block_on(disk.read(0, &mut readback)).expect("Read after COMRESET should succeed");
    assert_eq!(readback, pattern);
}

#[test]
fn test_ahci_multi_prdt_256k_scatter_gather_read_write_and_flush() {
    // 4096 sectors = 2 MiB disk
    let hal = MockAhciHal::new(4096, 0x0000_0133, 0x50);
    let mut disk = block_on(AhciDisk::open(dummy_ahci_pci(), ABAR, 0, hal.clone()))
        .expect("AhciDisk::open should succeed");

    // 512 KiB write and read (spans 128 4-KiB pages -> two 256 KiB multi-PRDT commands)
    let len = 512 * 1024;
    let write_buf: Vec<u8> = (0..len)
        .map(|i| ((i * 29 + (i >> 9) * 7) & 0xFF) as u8)
        .collect();
    block_on(disk.write(64 * 1024, &write_buf)).expect("512 KiB write should succeed");

    let mut read_buf = vec![0u8; len];
    block_on(disk.read(64 * 1024, &mut read_buf)).expect("512 KiB read should succeed");
    assert_eq!(read_buf, write_buf);

    // Unaligned byte offset and length read-modify-write
    let unaligned_off = 64 * 1024 + 211;
    let unaligned_patch: Vec<u8> = (0..3333).map(|i| ((i * 41 + 0x77) & 0xFF) as u8).collect();
    block_on(disk.write(unaligned_off, &unaligned_patch)).expect("Unaligned write should succeed");
    let mut unaligned_read = vec![0u8; unaligned_patch.len()];
    block_on(disk.read(unaligned_off, &mut unaligned_read)).expect("Unaligned read should succeed");
    assert_eq!(unaligned_read, unaligned_patch);

    // Verify prefix before unaligned_off was preserved
    let mut prefix = vec![0u8; 211];
    block_on(disk.read(64 * 1024, &mut prefix)).unwrap();
    assert_eq!(&prefix[..], &write_buf[..211]);

    // Flush
    block_on(disk.flush()).expect("AHCI flush should succeed");

    let st = hal.state.lock().unwrap();
    assert!(
        st.max_prdtl_seen > 1,
        "Expected scatter-gather multi-PRDT transfers (max_prdtl_seen > 1), got {}",
        st.max_prdtl_seen
    );
    assert!(
        st.flush_count >= 1,
        "Expected FLUSH CACHE (EXT) command to be issued"
    );
}
