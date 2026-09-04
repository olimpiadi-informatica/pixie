use crate::UDP_BODY_LEN;
use alloc::{vec, vec::Vec};
use thiserror::Error;

const PACKET_LEN: usize = UDP_BODY_LEN - 32;
const HEADER_LEN: usize = 2;
const BODY_LEN: usize = PACKET_LEN - HEADER_LEN;

const MIN_SIZE: usize = HEADER_LEN;
const MAX_SIZE: usize = PACKET_LEN;

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
    missing_packet: Vec<bool>,
    missing_packets_per_group: [u16; 32],
    missing_groups: u16,
}

impl Decoder {
    pub fn new(size: usize) -> Self {
        let num_packets = size.div_ceil(BODY_LEN);
        let data = vec![0; 32 * BODY_LEN + size];
        let missing_packet = vec![true; 32 + num_packets];
        let missing_packets_per_group: [u16; 32] = (0..32)
            .map(|i| ((num_packets + 31 - i) / 32) as u16)
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();
        let missing_groups = missing_packets_per_group
            .iter()
            .map(|&x| (x != 0) as u16)
            .sum();
        Decoder {
            data,
            missing_packet,
            missing_packets_per_group,
            missing_groups,
        }
    }

    /// The exact body length (excluding the 2-byte index header) a well-formed packet at
    /// `rot_index` must have, given the total decoded `size` this decoder was built for.
    ///
    /// Both parity slots (`rot_index < 32`) and real data slots (`rot_index >= 32`) share the
    /// same formula: a slot's body is a full [`BODY_LEN`], except when it lands on the chunk's
    /// final, partial block, in which case it is only the remaining bytes. This mirrors
    /// [`Encoder::next_packet`], which is what actually decides the length of every packet.
    fn expected_body_len(&self, rot_index: usize) -> usize {
        let size = self.data.len() - 32 * BODY_LEN;
        let pos = if rot_index < 32 {
            rot_index
        } else {
            rot_index - 32
        };
        BODY_LEN.min(size.saturating_sub(pos * BODY_LEN))
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
        if rot_index >= self.missing_packet.len() {
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

        let missing = &mut self.missing_packet[rot_index];
        match missing {
            false => return Ok(()),
            x @ true => *x = false,
        }

        let start = rot_index * BODY_LEN;
        self.data[start..start + buf.len() - 2].clone_from_slice(&buf[2..]);

        let group = index & 31;
        match &mut self.missing_packets_per_group[group as usize] {
            0 => return Ok(()),
            x @ 1 => *x = 0,
            x @ 2.. => {
                *x -= 1;
                return Ok(());
            }
        }

        match &mut self.missing_groups {
            0 => unreachable!(),
            x @ 1.. => *x -= 1,
        }

        Ok(())
    }

    pub fn finish(&mut self) -> Option<Vec<u8>> {
        if self.missing_groups != 0 {
            return None;
        }

        let mut xor = [[0; BODY_LEN]; 32];
        for packet in 0..self.missing_packet.len() {
            if !self.missing_packet[packet] {
                let group = packet & 31;
                self.data[BODY_LEN * packet..]
                    .iter()
                    .zip(xor[group].iter_mut())
                    .for_each(|(a, b)| *b ^= a);
            }
        }
        for packet in 0..self.missing_packet.len() {
            if self.missing_packet[packet] {
                let group = packet & 31;
                self.data[BODY_LEN * packet..]
                    .iter_mut()
                    .zip(xor[group].iter())
                    .for_each(|(a, b)| *a = *b);
            }
        }
        Some(self.data[32 * BODY_LEN..].to_vec())
    }
}

pub struct Encoder {
    data: Vec<u8>,
    groups: usize,
    idx: usize,
}

impl Encoder {
    pub fn new(data: Vec<u8>) -> Self {
        let idx = 0;
        let groups = data.len().div_ceil(BODY_LEN).min(32);
        Encoder { data, groups, idx }
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
            self.groups -= 1;
            out_buf[0..2].copy_from_slice(&(self.groups as u16).wrapping_sub(32).to_le_bytes());
            out_buf[2..].fill(0);
            (0..)
                .map(|x| (x * 32 + self.groups) * BODY_LEN)
                .take_while(|x| *x < self.data.len())
                .for_each(|start| {
                    let end = self.data.len().min(start + BODY_LEN);
                    out_buf[2..2 + end - start]
                        .iter_mut()
                        .zip(self.data[start..end].iter())
                        .for_each(|(a, b)| *a ^= *b);
                });
            Some(2 + BODY_LEN.min(self.data.len() - self.groups * BODY_LEN))
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
