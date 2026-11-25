#![no_std]
#![deny(unused_must_use)]

#[macro_use]
extern crate alloc;

pub use pixie_kernel as os;
pub use pixie_kernel::power_control;

#[cfg(feature = "coverage")]
pub mod export_cov;
pub mod flash;
pub mod parse_disk;
pub mod register;
pub mod rpc;
pub mod store;

// Memory to keep free for non-chunk storage.
pub const MIN_MEMORY: u64 = 32 << 20;

/// Returns `true` iff every byte in `buf` is zero using SSE4.1 `por`+`ptest`.
pub(crate) fn is_all_zero(buf: &[u8]) -> bool {
    // SAFETY: All supported build/runtime x86_64 targets in pixie have SSE4.1 enabled.
    unsafe { is_all_zero_sse41(buf) }
}

#[target_feature(enable = "sse4.1")]
unsafe fn is_all_zero_sse41(buf: &[u8]) -> bool {
    use core::arch::x86_64::{__m128i, _mm_loadu_si128, _mm_or_si128, _mm_testz_si128};

    let (chunks64, rem64) = buf.as_chunks::<64>();
    for chunk in chunks64 {
        let ptr = chunk.as_ptr() as *const __m128i;
        // SAFETY: `as_chunks::<64>()` guarantees 64 valid readable bytes; `_mm_loadu_si128` allows unaligned pointers.
        let (a, b, c, d) = unsafe {
            (
                _mm_loadu_si128(ptr),
                _mm_loadu_si128(ptr.add(1)),
                _mm_loadu_si128(ptr.add(2)),
                _mm_loadu_si128(ptr.add(3)),
            )
        };
        let combined = _mm_or_si128(_mm_or_si128(a, b), _mm_or_si128(c, d));
        if _mm_testz_si128(combined, combined) == 0 {
            return false;
        }
    }
    let (chunks16, rem16) = rem64.as_chunks::<16>();
    for chunk in chunks16 {
        let ptr = chunk.as_ptr() as *const __m128i;
        // SAFETY: `as_chunks::<16>()` guarantees 16 valid readable bytes; `_mm_loadu_si128` allows unaligned pointers.
        let v = unsafe { _mm_loadu_si128(ptr) };
        if _mm_testz_si128(v, v) == 0 {
            return false;
        }
    }
    rem16.iter().all(|&b| b == 0)
}

/// Computes the BLAKE3 hash of `data`, yielding between 256 KiB slices so RX polling continues.
pub(crate) async fn hash_cooperative(data: &[u8]) -> pixie_shared::ChunkHash {
    const HASH_SLICE: usize = 256 * 1024;
    let mut hasher = blake3::Hasher::new();
    let mut chunks = data.chunks(HASH_SLICE);
    if let Some(first) = chunks.next() {
        hasher.update(first);
    }
    for slice in chunks {
        crate::os::executor::Executor::sched_yield().await;
        hasher.update(slice);
    }
    hasher.finalize().into()
}

/// Groups consecutive items into contiguous `(start_offset, total_bytes, index_range)` batches
/// capped at `max_batch_bytes`.
pub(crate) fn build_contiguous_batches<T>(
    items: &[T],
    max_batch_bytes: usize,
    offset_and_size: impl Fn(&T) -> (usize, usize),
) -> alloc::vec::Vec<(u64, usize, core::ops::Range<usize>)> {
    let mut batches: alloc::vec::Vec<(u64, usize, core::ops::Range<usize>)> =
        alloc::vec::Vec::new();
    for (idx, item) in items.iter().enumerate() {
        let (offset, size) = offset_and_size(item);
        let off_u64 = offset as u64;
        if let Some((b_off, b_len, b_range)) = batches.last_mut()
            && off_u64 == *b_off + (*b_len as u64)
            && b_len
                .checked_add(size)
                .is_some_and(|s| s <= max_batch_bytes)
        {
            *b_len += size;
            b_range.end = idx + 1;
        } else {
            batches.push((off_u64, size, idx..idx + 1));
        }
    }
    batches
}

/// Computes the BLAKE3 hash of `len` zero bytes without allocating a `len`-byte buffer.
pub(crate) fn zero_hash(len: usize) -> pixie_shared::ChunkHash {
    static ZERO_BLOCK: [u8; 65536] = [0u8; 65536];
    let mut hasher = blake3::Hasher::new();
    let mut rem = len;
    while rem > 0 {
        let n = rem.min(ZERO_BLOCK.len());
        hasher.update(&ZERO_BLOCK[..n]);
        rem -= n;
    }
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_all_zero_edge_lengths_and_positions() {
        for len in [0, 1, 15, 16, 63, 64, 65, 127, 128, 4096] {
            let mut buf = vec![0u8; len];
            assert!(is_all_zero(&buf), "expected all-zero for len {len}");
            for pos in [0, len / 2, len.saturating_sub(1)] {
                if len == 0 {
                    continue;
                }
                buf[pos] = 0x80;
                assert!(
                    !is_all_zero(&buf),
                    "expected non-zero at pos {pos} for len {len}"
                );
                buf[pos] = 0;
            }
        }
    }

    #[test]
    fn test_zero_hash_matches_blake3_all_zero() {
        for len in [0, 1, 4096, 65536, 100_000] {
            let expected: pixie_shared::ChunkHash = blake3::hash(&vec![0u8; len]).into();
            assert_eq!(zero_hash(len), expected, "zero_hash mismatch for len {len}");
        }
    }

    #[test]
    fn test_build_contiguous_batches() {
        assert!(build_contiguous_batches::<(usize, usize)>(&[], 8192, |x| *x).is_empty());

        let items = [
            (0, 4096),
            (4096, 4096),
            (8192, 4096),   // hits 8192 cap, starts new batch
            (16384, 4096),  // non-contiguous gap from 12288, starts new batch
            (20480, 12288), // exceeds cap alone -> own batch
            (32768, 4096),
        ];
        let batches = build_contiguous_batches(&items, 8192, |x| *x);
        assert_eq!(
            batches,
            vec![
                (0, 8192, 0..2),
                (8192, 4096, 2..3),
                (16384, 4096, 3..4),
                (20480, 12288, 4..5),
                (32768, 4096, 5..6),
            ]
        );
    }
}
