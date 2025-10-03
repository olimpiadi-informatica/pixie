#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod bijection;
pub mod chunk_codec;
#[cfg(feature = "std")]
pub mod config;
pub mod util;

use alloc::{collections::BTreeMap, string::String, vec::Vec};
use blake3::OUT_LEN;
use core::fmt::Display;
use serde::{Deserialize, Serialize};
#[cfg(feature = "std")]
use std::{collections::HashMap, net::Ipv4Addr};

pub use bijection::Bijection;
#[cfg(feature = "std")]
pub use config::*;

/// Maximum size in bytes for a chunk.
pub const MAX_CHUNK_SIZE: usize = 1 << 22;

pub const ACTION_PORT: u16 = 25640;

/// udp port for chunk broadcasting.
pub const CHUNKS_PORT: u16 = ACTION_PORT + 1;
/// udp port for registration hint broadcasting.
pub const HINT_PORT: u16 = ACTION_PORT + 2;
/// udp port for pings from workstations.
pub const PING_PORT: u16 = ACTION_PORT + 3;

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
    /// Computes the size in bytes of an image.
    ///
    /// Repeated chunks are counted many times.
    pub fn size(&self) -> u64 {
        self.disk.iter().map(|chunk| chunk.size as u64).sum()
    }

    /// Computes the compressed size in bytes of an image.
    ///
    /// Repeated chunks are counted once.
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
pub struct RegistrationInfo {
    pub group: String,
    pub row: u8,
    pub col: u8,
    pub image: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HintPacket {
    pub station: RegistrationInfo,
    pub images: Vec<String>,
    pub groups: Bijection<String, u8>,
}

/// The maximum number of bytes in a udp packet with mtu 1500
pub const UDP_BODY_LEN: usize = 1472;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Reboot into the OS.
    Reboot,
    /// Shutdown the machine.
    Shutdown,
    /// Register the machine.
    Register,
    /// Make an image out of the current content of the disk and store it in the server database.
    Store,
    /// Fetch the image from the server and flash it to the disk.
    Flash,
    /// Wait for another command.
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

/// A request for the udp server.
///
/// To send a request to the server it must be serialized with postcard and sent
/// in a single udp packet at the address `<SERVER_IP>:25640`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UdpRequest {
    /// Used for server discovery.
    /// To be sent in broadcast over the lan.
    /// The server will reply with an empty packet.
    Discover,
    /// Sets the progress of the client for the current action.
    ActionProgress(usize, usize),
    /// Requests the given chunks to be broadcasted by the server.
    RequestChunks(Vec<ChunkHash>),
}

/// A request for the tcp server.
///
/// Over a single tcp connection multiple request can be sent.
/// Each request is encoded as:
/// - request length with 8 bytes;
/// - request content encoded with postcard;
///
/// To each request the server will reply with a message in the same format:
/// - request length with 8 bytes;
/// - response content encoded with postcard;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum TcpRequest {
    /// Checks if the server contains the chunk in the database.
    /// The server will reply with a bool.
    HasChunk(ChunkHash),
    /// Asks the server the [`Image`], the image name is deduced by the client configuration.
    /// The server replies with the requested [`Image`].
    GetImage,
    /// Registers the client with the given info.
    /// The response is empty.
    Register(RegistrationInfo),
    /// Uploads the given chunk to the server, the content is already compressed.
    /// The response is empty.
    UploadChunk(Vec<u8>),
    /// Uploads the [`Image`] to the server, the image name is deduced by the client configuration.
    /// The response is empty.
    UploadImage(Image),
    /// Asks the server for the client action to run.
    /// The server will reply with an [`Action`].
    GetAction,
    /// Tells the server that the action is complete and can proced to the next action.
    /// The response is emtpy.
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
