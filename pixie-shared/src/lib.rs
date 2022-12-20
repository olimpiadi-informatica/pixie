#![no_std]

extern crate alloc;

use alloc::{string::String, vec::Vec};
use blake3::OUT_LEN;
use serde::{Deserialize, Serialize};

pub const CHUNK_SIZE: usize = 1 << 22;

/// The hash of a chunk of a disk.
///
/// This is stored as an array of bytes because [`blake3::Hash`] is not serializable.
pub type ChunkHash = [u8; OUT_LEN];
/// The offset of the chunk of a disk.
pub type Offset = usize;

/// Describes one segment from a disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    pub hash: ChunkHash,
    pub start: Offset,
    pub size: usize,
    /// size after compression
    pub csize: usize,
}

/// An image is given by the list of chunks of the disk, the index of the boot entry that boots it,
/// and the contents of that boot entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Image {
    pub boot_option_id: u16,
    pub boot_entry: Vec<u8>,
    pub disk: Vec<Segment>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Station {
    pub group: u8,
    pub row: u8,
    pub col: u8,
    pub image: String,
}

pub const PACKET_LEN: usize = 1436;
pub const HEADER_LEN: usize = 34;
pub const BODY_LEN: usize = PACKET_LEN - HEADER_LEN;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Address {
    pub ip: (u8, u8, u8, u8),
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Action {
    Reboot,
    Register {
        hint_port: u16,
        server: Address,
    },
    Push {
        http_server: Address,
        image: String,
    },
    Pull {
        http_server: Address,
        image: String,
        udp_recv_port: u16,
        udp_server: Address,
    },
    Wait,
}
