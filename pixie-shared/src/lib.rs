use std::path::PathBuf;

use blake3::OUT_LEN;
use serde_derive::{Deserialize, Serialize};

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
}

/// A file is stored as a list of chunks and offsets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct File {
    pub name: PathBuf,
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
    pub row: u8,
    pub col: u8,
    pub group: u8,
}
