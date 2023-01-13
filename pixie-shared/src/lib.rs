#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::{string::String, vec::Vec};
use blake3::OUT_LEN;
use core::fmt::Write;
use serde::{Deserialize, Serialize};

#[cfg(feature = "std")]
use std::net::SocketAddrV4;

pub mod bijection;
pub use bijection::Bijection;

pub const CHUNK_SIZE: usize = 1 << 22;

pub const ACTION_PORT: u16 = 25640;

/// The hash of a chunk of a disk.
///
/// This is stored as an array of bytes because [`blake3::Hash`] is not serializable.
pub type ChunkHash = [u8; OUT_LEN];
/// The offset of the chunk of a disk.
pub type Offset = usize;

/// Describes one chunk from a disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub hash: ChunkHash,
    pub start: Offset,
    pub size: usize,
    /// Compressed size
    pub csize: usize,
}

/// An image is given by the list of chunks of the disk, the index of the boot entry that boots it,
/// and the contents of that boot entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Image {
    pub boot_option_id: u16,
    pub boot_entry: Vec<u8>,
    pub disk: Vec<Chunk>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Station {
    pub group: String,
    pub row: u8,
    pub col: u8,
    pub image: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HintPacket {
    pub station: Station,
    pub images: Vec<String>,
    pub groups: Bijection<String, u8>,
}

pub const PACKET_LEN: usize = 1436;
pub const HEADER_LEN: usize = 34;
pub const BODY_LEN: usize = PACKET_LEN - HEADER_LEN;

pub type Ip = [u8; 4];

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct Address {
    pub ip: Ip,
    pub port: u16,
}

#[cfg(feature = "std")]
impl From<Address> for SocketAddrV4 {
    fn from(addr: Address) -> Self {
        SocketAddrV4::new(addr.ip.into(), addr.port)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Action {
    Reboot,
    Register { hint_port: u16 },
    Push { image: String },
    Pull { image: String, chunks_port: u16 },
    Wait,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UdpRequest {
    GetAction,
    ActionProgress(usize, usize),
    ActionComplete,
    RequestChunks(Vec<ChunkHash>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TcpRequest {
    GetChunkSize(ChunkHash),
    GetImage(String),
    Register(Station),
    UploadChunk(ChunkHash, Vec<u8>),
    UploadImage(String, Image),
}

#[cfg(feature = "std")]
pub mod config;

#[cfg(feature = "std")]
pub use config::*;

pub fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::new();
    for byte in bytes {
        write!(s, "{:02x}", byte).unwrap();
    }
    s
}
