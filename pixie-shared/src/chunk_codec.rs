use crate::UDP_BODY_LEN;
use alloc::{vec, vec::Vec};
use core::mem;
use thiserror::Error;

const PACKET_LEN: usize = UDP_BODY_LEN - 32;
const HEADER_LEN: usize = 2;
const BODY_LEN: usize = PACKET_LEN - HEADER_LEN;

const MIN_SIZE: usize = HEADER_LEN;
const MAX_SIZE: usize = PACKET_LEN;

/// Primitive polynomial x^32 + x^22 + x^2 + x + 1 over GF(2) (period 2^32 - 1).
const POLY: u32 = (1 << 22) | (1 << 2) | (1 << 1) | 1;
const EXTRA_STEPS: usize = 256;

#[inline]
const fn step_mask(v: u32) -> u32 {
    let msb = (v >> 31) & 1;
    (v << 1) ^ (if msb != 0 { POLY } else { 0 })
}

#[inline]
const fn initial_mask(num_packets: usize) -> u32 {
    if num_packets < 32 {
        1
    } else {
        let mut v = 1u32;
        let mut steps = EXTRA_STEPS;
        while steps > 0 {
            v = step_mask(v);
            steps -= 1;
        }
        v
    }
}

#[inline]
fn xor_slice(dst: &mut [u8], src: &[u8]) {
    dst[..src.len()]
        .iter_mut()
        .zip(src)
        .for_each(|(a, b)| *a ^= *b);
}

#[inline]
fn step_parity(parity: &mut [u8], base: &mut usize, parity_len: usize) {
    *base = (*base + 31) & 31;
    let p0 = *base;
    for tap in [1, 2, 22] {
        let pt = (*base + tap) & 31;
        let (a, b) = if p0 < pt {
            let (left, right) = parity.split_at_mut(pt * parity_len);
            (
                &mut right[..parity_len],
                &left[p0 * parity_len..(p0 + 1) * parity_len],
            )
        } else {
            let (left, right) = parity.split_at_mut(p0 * parity_len);
            (
                &mut left[pt * parity_len..(pt + 1) * parity_len],
                &right[..parity_len],
            )
        };
        xor_slice(a, b);
    }
}

fn compute_parity(data: &[u8], missing_data: Option<&[bool]>) -> (Vec<u8>, usize) {
    let parity_len = BODY_LEN.min(data.len());
    let num_packets = data.len().div_ceil(BODY_LEN);
    let mut parity = vec![0u8; 32 * parity_len];
    let mut base = 0usize;

    for d in 0..num_packets {
        if d > 0 {
            step_parity(&mut parity, &mut base, parity_len);
        }
        if missing_data.is_none_or(|m| !m[d]) {
            let start = d * BODY_LEN;
            let end = data.len().min(start + BODY_LEN);
            let p_start = base * parity_len;
            xor_slice(
                &mut parity[p_start..p_start + parity_len],
                &data[start..end],
            );
        }
    }
    if num_packets >= 32 {
        for _ in 0..EXTRA_STEPS {
            step_parity(&mut parity, &mut base, parity_len);
        }
    }
    (parity, base)
}

#[derive(Error, Debug)]
pub enum DecoderError {
    #[error("Packet too small; got {0} bytes, expected at least {MIN_SIZE} bytes")]
    PacketTooSmall(usize),
    #[error("Packet too big; got {0} bytes, expected at most {MAX_SIZE} bytes")]
    PacketTooBig(usize),
    #[error("Invalid index: 0x{0:04x}")]
    InvalidIndex(u16),
    #[error(
        "Wrong packet size for index 0x{index:04x}: got {got} bytes, expected {expected} bytes"
    )]
    WrongSize {
        index: u16,
        expected: usize,
        got: usize,
    },
}

pub struct Decoder {
    data: Vec<u8>,
    missing_data: Vec<bool>,
    missing_data_count: usize,
    parity: Vec<u8>,
    received_parity_mask: u32,
}

impl Decoder {
    pub fn new(size: usize) -> Self {
        let num_packets = size.div_ceil(BODY_LEN);
        Decoder {
            data: vec![0; size],
            missing_data: vec![true; num_packets],
            missing_data_count: num_packets,
            parity: Vec::new(),
            received_parity_mask: 0,
        }
    }

    /// The exact body length (excluding the 2-byte index header) a well-formed packet at
    /// `rot_index` must have, given the total decoded `size` this decoder was built for.
    fn expected_body_len(&self, rot_index: usize) -> usize {
        let size = self.data.len();
        if rot_index < 32 {
            BODY_LEN.min(size)
        } else {
            let pos = rot_index - 32;
            BODY_LEN.min(size.saturating_sub(pos * BODY_LEN))
        }
    }

    pub fn add_packet(&mut self, buf: &[u8]) -> Result<(), DecoderError> {
        if buf.len() < MIN_SIZE {
            return Err(DecoderError::PacketTooSmall(buf.len()));
        }
        if buf.len() > MAX_SIZE {
            return Err(DecoderError::PacketTooBig(buf.len()));
        }

        let index = u16::from_le_bytes(buf[..2].try_into().unwrap());

        let rot_index = index.wrapping_add(32) as usize;
        if rot_index >= 32 + self.missing_data.len() {
            return Err(DecoderError::InvalidIndex(index));
        }

        let expected = self.expected_body_len(rot_index);
        let got = buf.len() - 2;
        if got != expected {
            return Err(DecoderError::WrongSize {
                index,
                expected,
                got,
            });
        }

        if rot_index < 32 {
            if self.missing_data_count == 0 {
                return Ok(());
            }
            let bit = 1u32 << rot_index;
            if self.received_parity_mask & bit == 0 {
                if self.parity.is_empty() {
                    self.parity.resize(32 * got, 0);
                }
                self.received_parity_mask |= bit;
                let start = rot_index * got;
                self.parity[start..start + got].copy_from_slice(&buf[2..]);
            }
        } else {
            let d = rot_index - 32;
            if self.missing_data[d] {
                self.missing_data[d] = false;
                self.missing_data_count -= 1;
                let start = d * BODY_LEN;
                self.data[start..start + got].copy_from_slice(&buf[2..]);
            }
        }

        Ok(())
    }

    pub fn finish(&mut self) -> Option<Vec<u8>> {
        if self.missing_data_count == 0 {
            return Some(mem::take(&mut self.data));
        }
        if self.missing_data_count > self.received_parity_mask.count_ones() as usize {
            return None;
        }

        let num_packets = self.missing_data.len();
        let mut missing_indices = [0usize; 32];
        let mut missing_masks = [0u32; 32];
        let mut m = 0usize;
        let mut cur_mask = initial_mask(num_packets);
        for (d, &is_missing) in self.missing_data.iter().enumerate().rev() {
            if is_missing {
                missing_indices[m] = d;
                missing_masks[m] = cur_mask;
                m += 1;
            }
            cur_mask = step_mask(cur_mask);
        }

        let mut pivot_lhs = [0u32; 32];
        let mut pivot_rhs = [0u32; 32];
        let mut rank = 0usize;

        for g in 0..32 {
            if (self.received_parity_mask >> g) & 1 == 0 {
                continue;
            }
            let mut lhs = 0u32;
            for (j, &mask) in missing_masks[..m].iter().enumerate() {
                lhs |= ((mask >> g) & 1) << j;
            }
            let mut rhs = 1u32 << g;

            for j in 0..m {
                if (lhs >> j) & 1 != 0 && pivot_lhs[j] != 0 {
                    lhs ^= pivot_lhs[j];
                    rhs ^= pivot_rhs[j];
                }
            }
            if lhs != 0 {
                let pivot = lhs.trailing_zeros() as usize;
                for j in 0..m {
                    if pivot_lhs[j] != 0 && (pivot_lhs[j] >> pivot) & 1 != 0 {
                        pivot_lhs[j] ^= lhs;
                        pivot_rhs[j] ^= rhs;
                    }
                }
                pivot_lhs[pivot] = lhs;
                pivot_rhs[pivot] = rhs;
                rank += 1;
                if rank == m {
                    break;
                }
            }
        }

        if rank < m {
            return None;
        }

        let parity_len = BODY_LEN.min(self.data.len());
        if m == 1 {
            // Fast path for single-packet recovery: accumulate directly into the missing slice
            // of `self.data` with zero allocations.
            let d0 = missing_indices[0];
            let start0 = d0 * BODY_LEN;
            let len0 = self.expected_body_len(32 + d0);
            let rhs = pivot_rhs[0];
            let mut acc = vec![0u8; parity_len];
            let mut r = rhs;
            while r != 0 {
                let g = r.trailing_zeros() as usize;
                r &= r - 1;
                let p_start = g * parity_len;
                xor_slice(&mut acc, &self.parity[p_start..p_start + parity_len]);
            }
            let mut mask = initial_mask(num_packets);
            for (d, &is_missing) in self.missing_data.iter().enumerate().rev() {
                if !is_missing && ((mask & rhs).count_ones() & 1 != 0) {
                    let s = d * BODY_LEN;
                    let e = self.data.len().min(s + BODY_LEN);
                    xor_slice(&mut acc, &self.data[s..e]);
                }
                mask = step_mask(mask);
            }
            self.data[start0..start0 + len0].copy_from_slice(&acc[..len0]);
        } else {
            let active_parity = pivot_rhs[..m].iter().fold(0u32, |acc, &r| acc | r);
            let (known_parity, base) = compute_parity(&self.data, Some(&self.missing_data));
            for g in 0..32 {
                if (active_parity >> g) & 1 != 0 {
                    let p_start = g * parity_len;
                    let k_start = ((base + g) & 31) * parity_len;
                    xor_slice(
                        &mut self.parity[p_start..p_start + parity_len],
                        &known_parity[k_start..k_start + parity_len],
                    );
                }
            }

            for j in 0..m {
                let d = missing_indices[j];
                let start = d * BODY_LEN;
                let len = self.expected_body_len(32 + d);
                let dst = &mut self.data[start..start + len];
                dst.fill(0);
                let mut rhs = pivot_rhs[j];
                while rhs != 0 {
                    let g = rhs.trailing_zeros() as usize;
                    rhs &= rhs - 1;
                    let p_start = g * parity_len;
                    xor_slice(dst, &self.parity[p_start..p_start + len]);
                }
            }
        }

        self.missing_data.fill(false);
        self.missing_data_count = 0;
        Some(mem::take(&mut self.data))
    }
}

pub struct Encoder {
    data: Vec<u8>,
    groups: usize,
    idx: usize,
    parity: Option<(Vec<u8>, usize)>,
}

impl Encoder {
    pub fn new(data: Vec<u8>) -> Self {
        let idx = 0;
        let groups = data.len().div_ceil(BODY_LEN).min(32);
        Encoder {
            data,
            groups,
            idx,
            parity: None,
        }
    }

    pub fn next_packet(&mut self, out_buf: &mut [u8]) -> Option<usize> {
        let start = self.idx * BODY_LEN;
        if start < self.data.len() {
            let end = self.data.len().min(start + BODY_LEN);
            let len = end - start;
            out_buf[0..2].copy_from_slice(&(self.idx as u16).to_le_bytes());
            out_buf[2..2 + len].copy_from_slice(&self.data[start..end]);
            self.idx += 1;
            Some(2 + len)
        } else if self.groups > 0 {
            let (parity, base) = self
                .parity
                .get_or_insert_with(|| compute_parity(&self.data, None));
            self.groups -= 1;
            let g = self.groups;
            let len = BODY_LEN.min(self.data.len());
            let p_start = ((*base + g) & 31) * len;
            out_buf[0..2].copy_from_slice(&(g as u16).wrapping_sub(32).to_le_bytes());
            out_buf[2..2 + len].copy_from_slice(&parity[p_start..p_start + len]);
            Some(2 + len)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UDP_BODY_LEN;

    fn test_chunk_skip_packet(chunk: &[u8]) {
        let mut encoder = Encoder::new(chunk.to_vec());
        let mut packets = Vec::new();
        let mut buf = [0u8; UDP_BODY_LEN];
        while let Some(len) = encoder.next_packet(&mut buf) {
            packets.push(buf[..len].to_vec());
        }

        packets.sort_by_key(|p| {
            p.iter().take(6).fold(0u64, |acc, &x| {
                acc.wrapping_mul(0x5DEECE66D).wrapping_add(x as u64)
            })
        });

        for skip_idx in 0..packets.len() {
            let mut decoder = Decoder::new(chunk.len());
            for (idx, packet) in packets.iter().enumerate() {
                if idx != skip_idx {
                    decoder.add_packet(packet).expect("Failed to add packet");
                }
            }
            let decoded = decoder.finish().expect("Failed to decode chunk");
            assert_eq!(
                decoded, chunk,
                "Failed to decode chunk with skip index {skip_idx}"
            );
        }

        let mut decoder = Decoder::new(chunk.len());
        for (idx, packet) in packets.iter().enumerate() {
            if !(idx.is_multiple_of(33) && idx / 33 < 32) {
                decoder.add_packet(packet).expect("Failed to add packet");
            }
        }
        let decoded = decoder.finish().expect("Failed to decode chunk");
        assert_eq!(decoded, chunk, "Failed to decode chunk with multiple skips");
    }

    #[test]
    fn test_small_chunk() {
        let mut chunk = vec![0u8; 20];
        let mut val = u64::MAX / 5;
        for x in &mut chunk {
            val = val.wrapping_mul(0x5DEECE66D).wrapping_add(0xB);
            *x = val.to_be_bytes()[0];
        }
        test_chunk_skip_packet(&chunk);
    }

    #[test]
    fn test_big_chunk() {
        let mut chunk = vec![0u8; 200 << 10];
        let mut val = u64::MAX / 5;
        for x in &mut chunk {
            val = val.wrapping_mul(0x5DEECE66D).wrapping_add(0xB);
            *x = val.to_be_bytes()[0];
        }
        test_chunk_skip_packet(&chunk);
    }

    fn packet_for(index: u16, body_len: usize) -> Vec<u8> {
        let mut packet = vec![0xAB; 2 + body_len];
        packet[..2].copy_from_slice(&index.to_le_bytes());
        packet
    }

    /// A malicious peer claiming to be the chunk's final (and thus short) data block, but
    /// sending a full-size body, used to make `add_packet` write past the end of `data` and
    /// panic. It must now be rejected instead.
    #[test]
    fn test_oversized_final_block_is_rejected_not_panicking() {
        let size = 2 * BODY_LEN + 124;
        let mut decoder = Decoder::new(size);
        let last_index = (size.div_ceil(BODY_LEN) - 1) as u16;

        let oversized = packet_for(last_index, BODY_LEN);
        let err = decoder
            .add_packet(&oversized)
            .expect_err("an oversized packet for the final block must be rejected, not accepted");
        assert!(matches!(err, DecoderError::WrongSize { .. }));

        // The slot must still be genuinely missing: a correctly-sized packet is accepted after.
        let correct = packet_for(last_index, 124);
        decoder
            .add_packet(&correct)
            .expect("correctly-sized packet must still be accepted");
    }

    /// A malicious/corrupt peer sending fewer bytes than a slot actually needs used to be
    /// accepted silently, leaving the remainder of that block as stale zero bytes in the
    /// reconstructed chunk. It must now be rejected instead.
    #[test]
    fn test_undersized_packet_is_rejected_not_silently_corrupting() {
        let size = 3 * BODY_LEN;
        let mut decoder = Decoder::new(size);

        let undersized = packet_for(0, 10);
        let err = decoder
            .add_packet(&undersized)
            .expect_err("an undersized packet must be rejected, not silently accepted");
        assert!(matches!(err, DecoderError::WrongSize { .. }));

        // The slot must still be genuinely missing: a correctly-sized packet is accepted after.
        let correct = packet_for(0, BODY_LEN);
        decoder
            .add_packet(&correct)
            .expect("correctly-sized packet must still be accepted");
    }

    /// For every kind of slot a decoder can have (full data blocks, the chunk's one short final
    /// block, and parity blocks - including a short parity block, which happens whenever the
    /// chunk fits in fewer than 32 blocks) any body length other than the exact expected one
    /// must be rejected, never silently accepted and never panic.
    #[test]
    fn test_every_slot_rejects_every_wrong_size() {
        for size in [
            124,
            BODY_LEN,
            BODY_LEN + 1,
            2 * BODY_LEN + 124,
            40 * BODY_LEN + 7,
        ] {
            let num_packets = size.div_ceil(BODY_LEN);
            for rot_index in 0..32 + num_packets {
                let probe = Decoder::new(size);
                let expected = probe.expected_body_len(rot_index);
                let index = (rot_index as u16).wrapping_sub(32);

                for wrong_len in [0, expected.saturating_sub(1), expected + 1, BODY_LEN] {
                    if wrong_len == expected {
                        continue;
                    }
                    let mut decoder = Decoder::new(size);
                    let packet = packet_for(index, wrong_len);
                    assert!(
                        decoder.add_packet(&packet).is_err(),
                        "size {size}, rot_index {rot_index}: wrong length {wrong_len} \
                         (expected {expected}) was accepted"
                    );
                }

                // The exact expected length is always accepted.
                let mut decoder = Decoder::new(size);
                let packet = packet_for(index, expected);
                decoder
                    .add_packet(&packet)
                    .unwrap_or_else(|e| panic!("size {size}, rot_index {rot_index}: {e}"));
            }
        }
    }
}

/// Micro-benchmark for the codec itself, not a stand-in for a real flash: it loops
/// [`Encoder::next_packet`] straight into [`Decoder::add_packet`] over an in-memory buffer,
/// skipping the real UEFI/tokio UDP sockets and the rest of the flash pipeline entirely. Run and
/// tracked in CI (`cargo +nightly bench --features std`, see `.github/workflows/rust.yml`) to
/// catch regressions in this hot, security-sensitive path in isolation; see `run_test.sh` for the
/// actual end-to-end store/flash benchmark, which drives the real `pixie-uefi.efi` client in QEMU
/// against a real `pixie-server`.
#[cfg(test)]
mod benches {
    use super::*;
    use crate::UDP_BODY_LEN;
    use std::hint::black_box;
    use test::Bencher;

    fn make_chunk(size: usize) -> Vec<u8> {
        let mut chunk = vec![0u8; size];
        let mut val = u64::MAX / 5;
        for x in &mut chunk {
            val = val.wrapping_mul(0x5DEECE66D).wrapping_add(0xB);
            *x = val.to_be_bytes()[0];
        }
        chunk
    }

    fn bench_roundtrip(b: &mut Bencher, size: usize) {
        let chunk = make_chunk(size);
        let mut buf = [0u8; UDP_BODY_LEN];
        b.iter(|| {
            let mut encoder = Encoder::new(chunk.clone());
            let mut decoder = Decoder::new(chunk.len());
            while let Some(len) = encoder.next_packet(&mut buf) {
                decoder
                    .add_packet(&buf[..len])
                    .expect("failed to add packet");
            }
            black_box(decoder.finish().expect("failed to decode chunk"))
        });
    }

    #[bench]
    fn bench_flash_roundtrip_20b(b: &mut Bencher) {
        bench_roundtrip(b, 20);
    }

    #[bench]
    fn bench_flash_roundtrip_200kib(b: &mut Bencher) {
        bench_roundtrip(b, 200 << 10);
    }

    #[bench]
    fn bench_flash_roundtrip_4mib(b: &mut Bencher) {
        bench_roundtrip(b, crate::MAX_CHUNK_SIZE);
    }
}
