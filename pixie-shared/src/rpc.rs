use alloc::{vec, vec::Vec};
use core::ops::Range;
use serde::{Deserialize, Serialize};

use crate::{ChunkHash, RegistrationInfo, UDP_BODY_LEN, UnitStats};

/// Payload bytes per `BlobData` packet (fits comfortably inside `UDP_BODY_LEN = 1472` with postcard framing).
pub const RPC_BLOCK_SIZE: usize = 1400;
/// Maximum blob transfer size (16 MiB), matching the server's allocation cap.
pub const MAX_RPC_BLOB_LEN: usize = 16 * 1024 * 1024;
/// Maximum number of hashes batched into a single `SmallRpc::HasChunks` UDP packet.
pub const HAS_CHUNKS_BATCH_SIZE: usize = 40;
/// Maximum serialized RPC UDP packet length.
pub const MAX_RPC_PACKET_LEN: usize = UDP_BODY_LEN;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SmallRpc {
    GetAction,
    ActionComplete,
    Register(RegistrationInfo, UnitStats),
    HasChunks(Vec<ChunkHash>),
    GetImageInfo,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum UploadKind {
    Chunk,
    Image,
}

mod raw_bytes {
    use core::fmt;
    use serde::{
        Deserializer, Serializer,
        de::{Error, Visitor},
    };

    pub fn serialize<S: Serializer>(data: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(data)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<&'de [u8], D::Error> {
        struct BorrowedBytesVisitor;

        impl<'de> Visitor<'de> for BorrowedBytesVisitor {
            type Value = &'de [u8];

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a borrowed byte slice")
            }

            fn visit_borrowed_bytes<E: Error>(self, v: &'de [u8]) -> Result<Self::Value, E> {
                Ok(v)
            }
        }

        deserializer.deserialize_bytes(BorrowedBytesVisitor)
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum RpcPacket<'a> {
    SmallReq {
        nonce: u64,
        req_id: u32,
        op: SmallRpc,
    },
    SmallResp {
        req_id: u32,
        ok: bool,
        #[serde(borrow, with = "raw_bytes")]
        payload: &'a [u8],
    },
    BlobData {
        nonce: u64,
        req_id: u32,
        kind: UploadKind,
        total_len: u32,
        block_idx: u16,
        #[serde(borrow, with = "raw_bytes")]
        data: &'a [u8],
    },
    BlobAck {
        req_id: u32,
        ack_base: u16,
        sack_mask: u64,
    },
    BlobDone {
        req_id: u32,
        #[serde(borrow)]
        err: Option<&'a str>,
    },
}

/// Truncates an error message to fit inside a single UDP RPC response packet.
pub fn truncate_err(msg: &str) -> &str {
    let mut end = msg.len().min(MAX_RPC_PACKET_LEN - 32);
    while !msg.is_char_boundary(end) {
        end -= 1;
    }
    &msg[..end]
}

pub fn expected_block_len(total_len: usize, num_blocks: usize, block_idx: usize) -> usize {
    if block_idx + 1 < num_blocks {
        RPC_BLOCK_SIZE
    } else {
        total_len - block_idx * RPC_BLOCK_SIZE
    }
}

/// Fixed-window selective-ACK sender for a single blob upload.
#[derive(Debug)]
pub struct SackSender {
    total_len: usize,
    num_blocks: usize,
    ack_base: usize,
    next_unsent: usize,
    acked: Vec<bool>,
    /// `0` = unsent, `< 0` = marked for retransmit, `> 0` = last transmit timestamp in µs.
    sent_at_us: Vec<i64>,
}

impl SackSender {
    const WINDOW: usize = 64;

    pub fn new(total_len: usize) -> Self {
        let num_blocks = total_len.div_ceil(RPC_BLOCK_SIZE);
        Self {
            total_len,
            num_blocks,
            ack_base: 0,
            next_unsent: 0,
            acked: vec![false; num_blocks],
            sent_at_us: vec![0; num_blocks],
        }
    }

    pub fn is_complete(&self) -> bool {
        self.ack_base >= self.num_blocks
    }

    pub fn block_range(&self, block_idx: u16) -> Range<usize> {
        let start = block_idx as usize * RPC_BLOCK_SIZE;
        let end = (start + RPC_BLOCK_SIZE).min(self.total_len);
        start..end
    }

    pub fn next_block_to_send(&mut self, now_us: i64) -> Option<u16> {
        let limit = (self.ack_base + Self::WINDOW).min(self.num_blocks);
        let stamp = now_us.max(1);
        for idx in self.ack_base..limit {
            if !self.acked[idx] && self.sent_at_us[idx] < 0 {
                self.sent_at_us[idx] = stamp;
                return Some(idx as u16);
            }
        }
        if self.next_unsent < limit {
            let idx = self.next_unsent;
            self.next_unsent += 1;
            self.sent_at_us[idx] = stamp;
            return Some(idx as u16);
        }
        None
    }

    pub fn on_ack(&mut self, ack_base: u16, sack_mask: u64, now_us: i64) {
        let ab = (ack_base as usize).min(self.num_blocks);
        if ab > self.ack_base {
            self.acked[self.ack_base..ab].fill(true);
            self.ack_base = ab;
            self.next_unsent = self.next_unsent.max(ab);
        }

        let mut highest_acked = ab;
        for k in 0..64 {
            let idx = ab + 1 + k;
            if idx >= self.num_blocks {
                break;
            }
            if (sack_mask >> k) & 1 != 0 {
                self.acked[idx] = true;
                highest_acked = highest_acked.max(idx + 1);
            }
        }

        for idx in self.ack_base..highest_acked {
            if !self.acked[idx] && self.sent_at_us[idx] > 0 {
                // Trigger selective retransmit once 2 later blocks are SACKed or after 2 ms grace.
                let blocks_ahead = highest_acked - 1 - idx;
                if blocks_ahead >= 2 || now_us - self.sent_at_us[idx] >= 2_000 {
                    self.sent_at_us[idx] = -1;
                }
            }
        }
    }

    pub fn on_timeout(&mut self, now_us: i64, rto_us: i64) -> bool {
        let limit = (self.ack_base + Self::WINDOW).min(self.num_blocks);
        let mut any = false;
        for idx in self.ack_base..limit {
            if !self.acked[idx]
                && self.sent_at_us[idx] > 0
                && now_us - self.sent_at_us[idx] >= rto_us
            {
                self.sent_at_us[idx] = -1;
                any = true;
            }
        }
        any
    }
}

/// Selective-ACK receiver that assembles an incoming blob directly into a pre-sized buffer.
#[derive(Debug)]
pub struct SackReceiver {
    total_len: usize,
    num_blocks: usize,
    data: Vec<u8>,
    received: Vec<bool>,
    ack_base: usize,
    highest_seen: usize,
    unacked_pkts: usize,
}

impl SackReceiver {
    pub fn new(total_len: usize) -> Result<Self, &'static str> {
        if total_len == 0 || total_len > MAX_RPC_BLOB_LEN {
            return Err("invalid blob length");
        }
        let num_blocks = total_len.div_ceil(RPC_BLOCK_SIZE);
        Ok(Self {
            total_len,
            num_blocks,
            data: vec![0u8; total_len],
            received: vec![false; num_blocks],
            ack_base: 0,
            highest_seen: 0,
            unacked_pkts: 0,
        })
    }

    pub fn is_complete(&self) -> bool {
        self.ack_base >= self.num_blocks
    }

    pub fn ack_base(&self) -> u16 {
        self.ack_base as u16
    }

    pub fn sack_mask(&mut self) -> u64 {
        self.unacked_pkts = 0;
        let mut mask = 0u64;
        for k in 0..64 {
            let idx = self.ack_base + 1 + k;
            if idx >= self.num_blocks {
                break;
            }
            if self.received[idx] {
                mask |= 1u64 << k;
            }
        }
        mask
    }

    /// Processes a `BlobData` block, returning `true` when an ACK should be sent.
    pub fn on_data(&mut self, block_idx: u16, payload: &[u8]) -> Result<bool, &'static str> {
        let idx = block_idx as usize;
        if idx >= self.num_blocks {
            return Err("block_idx out of bounds");
        }
        let expected = expected_block_len(self.total_len, self.num_blocks, idx);
        if payload.len() != expected {
            return Err("unexpected block payload length");
        }
        if !self.received[idx] {
            let start = idx * RPC_BLOCK_SIZE;
            self.data[start..start + expected].copy_from_slice(payload);
            self.received[idx] = true;
            while self.ack_base < self.num_blocks && self.received[self.ack_base] {
                self.ack_base += 1;
            }
        }
        self.highest_seen = self.highest_seen.max(idx + 1);
        self.unacked_pkts += 1;
        let has_gap = self.ack_base < self.highest_seen;
        Ok(self.is_complete() || self.unacked_pkts >= 16 || (has_gap && self.unacked_pkts >= 4))
    }

    pub fn take_data(&mut self) -> Option<Vec<u8>> {
        self.is_complete().then(|| core::mem::take(&mut self.data))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, RngCore, SeedableRng, rngs::StdRng};

    #[test]
    fn test_packet_roundtrip() {
        let mut buf = [0u8; MAX_RPC_PACKET_LEN];
        let small = postcard::to_allocvec(&RpcPacket::SmallReq {
            nonce: 42,
            req_id: 7,
            op: SmallRpc::GetImageInfo,
        })
        .unwrap();
        assert_eq!(
            postcard::from_bytes::<RpcPacket>(&small).unwrap(),
            RpcPacket::SmallReq {
                nonce: 42,
                req_id: 7,
                op: SmallRpc::GetImageInfo,
            }
        );

        let resp = postcard::to_allocvec(&RpcPacket::SmallResp {
            req_id: 7,
            ok: true,
            payload: b"hello",
        })
        .unwrap();
        assert_eq!(
            postcard::from_bytes::<RpcPacket>(&resp).unwrap(),
            RpcPacket::SmallResp {
                req_id: 7,
                ok: true,
                payload: b"hello",
            }
        );

        let full_block = [0xABu8; RPC_BLOCK_SIZE];
        let encoded = postcard::to_slice(
            &RpcPacket::BlobData {
                nonce: u64::MAX,
                req_id: u32::MAX,
                kind: UploadKind::Image,
                total_len: MAX_RPC_BLOB_LEN as u32,
                block_idx: u16::MAX,
                data: &full_block,
            },
            &mut buf,
        )
        .unwrap();
        assert!(encoded.len() <= MAX_RPC_PACKET_LEN);
        assert_eq!(
            postcard::from_bytes::<RpcPacket>(encoded).unwrap(),
            RpcPacket::BlobData {
                nonce: u64::MAX,
                req_id: u32::MAX,
                kind: UploadKind::Image,
                total_len: MAX_RPC_BLOB_LEN as u32,
                block_idx: u16::MAX,
                data: &full_block,
            }
        );

        let ack = postcard::to_slice(
            &RpcPacket::BlobAck {
                req_id: 3,
                ack_base: 10,
                sack_mask: 0xA5,
            },
            &mut buf,
        )
        .unwrap();
        assert_eq!(
            postcard::from_bytes::<RpcPacket>(ack).unwrap(),
            RpcPacket::BlobAck {
                req_id: 3,
                ack_base: 10,
                sack_mask: 0xA5,
            }
        );

        let long_err = "é".repeat(1000);
        let trunc = truncate_err(&long_err);
        let done_err = postcard::to_allocvec(&RpcPacket::BlobDone {
            req_id: 3,
            err: Some(trunc),
        })
        .unwrap();
        assert!(done_err.len() <= MAX_RPC_PACKET_LEN);
        assert_eq!(
            postcard::from_bytes::<RpcPacket>(&done_err).unwrap(),
            RpcPacket::BlobDone {
                req_id: 3,
                err: Some(trunc),
            }
        );
    }

    #[test]
    fn test_sack_edge_cases() {
        assert!(SackReceiver::new(0).is_err());
        assert!(SackReceiver::new(MAX_RPC_BLOB_LEN + 1).is_err());

        // Reject wrong payload length or out-of-range block
        let mut rx = SackReceiver::new(RPC_BLOCK_SIZE + 5).unwrap();
        assert!(rx.on_data(0, &[1u8; 10]).is_err());
        assert!(rx.on_data(1, &[1u8; 6]).is_err());
        assert!(rx.on_data(2, &[1u8; 5]).is_err());
    }

    #[test]
    fn test_sack_transfer_with_loss_and_reordering() {
        for (size, drop_prob) in [
            (1, 0.2),
            (RPC_BLOCK_SIZE, 0.15),
            (500_123, 0.10),
            (4 * 1024 * 1024 + 77, 0.15),
        ] {
            let mut rng = StdRng::seed_from_u64(0xBEEF_0000 + size as u64);
            let mut src = vec![0u8; size];
            rng.fill_bytes(&mut src);

            let mut now = 1_000i64;
            let mut tx = SackSender::new(size);
            let mut rx = SackReceiver::new(size).unwrap();
            let mut in_flight_pkts: Vec<(u16, Vec<u8>)> = Vec::new();

            for _ in 0..5000 {
                if tx.is_complete() && rx.is_complete() {
                    break;
                }
                while let Some(idx) = tx.next_block_to_send(now) {
                    if !rng.random_bool(drop_prob) {
                        let range = tx.block_range(idx);
                        in_flight_pkts.push((idx, src[range].to_vec()));
                    }
                }

                // Inject some reordering
                if in_flight_pkts.len() >= 4 && rng.random_bool(0.3) {
                    let i = rng.random_range(0..in_flight_pkts.len());
                    let j = rng.random_range(0..in_flight_pkts.len());
                    in_flight_pkts.swap(i, j);
                }

                let mut send_ack = false;
                for (idx, payload) in in_flight_pkts.drain(..) {
                    send_ack |= rx.on_data(idx, &payload).unwrap();
                }

                if send_ack && !rng.random_bool(drop_prob) {
                    let ab = rx.ack_base();
                    let mask = rx.sack_mask();
                    tx.on_ack(ab, mask, now);
                } else {
                    tx.on_timeout(now, 25_000);
                }
                now += 5_000;
            }

            assert!(tx.is_complete(), "tx incomplete for size {size}");
            assert!(rx.is_complete(), "rx incomplete for size {size}");
            assert_eq!(rx.take_data().unwrap(), src);
        }
    }
}
