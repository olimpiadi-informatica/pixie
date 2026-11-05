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
pub mod store;

// Memory to keep free for non-chunk storage.
pub const MIN_MEMORY: u64 = 32 << 20;
