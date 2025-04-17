#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod bijection;
#[cfg(feature = "std")]
pub mod config;

use alloc::{collections::BTreeMap, string::String, vec::Vec};
use blake3::OUT_LEN;
use core::fmt::Display;
use serde::{Deserialize, Serialize};
#[cfg(feature = "std")]
use std::{collections::HashMap, net::Ipv4Addr};

pub use bijection::Bijection;
#[cfg(feature = "std")]
pub use config::*;

pub const MAX_CHUNK_SIZE: usize = 1 << 22;

pub const ACTION_PORT: u16 = 25640;

pub const CHUNKS_PORT: u16 = 4041;
pub const HINT_PORT: u16 = 4042;
pub const PING_PORT: u16 = 4043;

/// The hash of a chunk of a disk.
///
/// This is stored as an array of bytes because [`blake3::Hash`] is not serializable.
pub type ChunkHash = [u8; OUT_LEN];
/// The offset of the chunk of a disk.
pub type Offset = usize;

/// Describes one chunk from a disk.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
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

impl Image {
    pub fn size(&self) -> u64 {
        self.disk.iter().map(|chunk| chunk.size as u64).sum()
    }

    pub fn csize(&self) -> u64 {
        let mut chunks: Vec<_> = self
            .disk
            .iter()
            .map(|chunk| (chunk.hash, chunk.csize))
            .collect();
        chunks.sort_unstable_by_key(|(hash, _)| *hash);
        chunks.dedup_by_key(|(hash, _)| *hash);
        chunks.into_iter().map(|(_, size)| size as u64).sum()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImagesStats {
    pub total_csize: u64,
    pub reclaimable: u64,
    /// size and csize
    pub images: BTreeMap<String, (u64, u64)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkStats {
    pub csize: u64,
    pub ref_cnt: usize,
}

pub type ChunksStats = BTreeMap<ChunkHash, ChunkStats>;

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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Reboot,
    Shutdown,
    Register,
    Store,
    Flash,
    Wait,
}

impl Display for Action {
    fn fmt(&self, fmt: &mut core::fmt::Formatter<'_>) -> Result<(), core::fmt::Error> {
        match self {
            Action::Reboot => write!(fmt, "reboot"),
            Action::Shutdown => write!(fmt, "shutdown"),
            Action::Register => write!(fmt, "register"),
            Action::Store => write!(fmt, "store"),
            Action::Flash => write!(fmt, "flash"),
            Action::Wait => write!(fmt, "wait"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UdpRequest {
    Discover,
    ActionProgress(usize, usize),
    RequestChunks(Vec<ChunkHash>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TcpRequest {
    HasChunk(ChunkHash),
    GetImage,
    Register(Station),
    UploadChunk(Vec<u8>),
    UploadImage(Image),
    GetAction,
    ActionComplete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg(feature = "std")]
pub enum StatusUpdate {
    Config(config::Config),
    HostMap(HashMap<Ipv4Addr, String>),
    Units(Vec<Unit>),
    ImagesStats(ImagesStats),
}
