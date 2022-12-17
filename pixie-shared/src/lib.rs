#![no_std]

extern crate alloc;

use alloc::{string::String, vec::Vec};
use blake3::OUT_LEN;
use serde::{Deserialize, Serialize};

pub const CHUNK_SIZE: usize = 1 << 22;

#[derive(Serialize, Deserialize)]
pub struct Group {
    pub name: String,
    pub shape: Option<(u8, u8)>,
}

#[derive(Serialize, Deserialize)]
pub struct RegistrationInfo {
    pub groups: Vec<Group>,
    pub candidate_group: String,
    pub candidate_position: Vec<u8>,
}

/// The hash of a chunk of a file.
///
/// This is stored as an array of bytes because [`blake3::Hash`] is not serializable.
pub type ChunkHash = [u8; OUT_LEN];
/// The offset of the chunk of a file.
pub type Offset = usize;

/// Describes one segment from a file
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    pub hash: ChunkHash,
    pub start: Offset,
    pub size: usize,
    /// size after compression
    pub csize: usize,
}

/// A file is stored as a list of chunks and offsets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct File {
    pub chunks: Vec<Segment>,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub enum StationKind {
    #[default]
    Worker,
    Contestant,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct Station {
    pub kind: StationKind,
    pub group: u8,
    pub row: u8,
    pub col: u8,
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
        server: Address,
    },
    Push {
        http_server: Address,
        path: String,
    },
    Pull {
        http_server: Address,
        path: String,
        udp_recv_port: u16,
        udp_server: Address,
    },
    Wait,
}
