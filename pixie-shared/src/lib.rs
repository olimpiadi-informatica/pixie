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
pub use serde::de::DeserializeOwned;

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
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
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

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UnitStats {
    pub ram: u64,
    pub disk: u64,
    pub cpu: String,
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
    Boot,
    /// Restart the machine.
    Restart,
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
            Action::Boot => write!(fmt, "reboot"),
            Action::Restart => write!(fmt, "restart"),
            Action::Shutdown => write!(fmt, "shutdown"),
            Action::Register => write!(fmt, "register"),
            Action::Store => write!(fmt, "store"),
            Action::Flash => write!(fmt, "flash"),
            Action::Wait => write!(fmt, "wait"),
        }
    }
}

/// Maximum slice payload size for [`UdpRequest::UploadSlice`].
///
/// Sized so the largest serialized `UploadSlice` (1B discriminant + <=3B `u16` id
/// + <=5B `u32` offset + 2B length varint + 1456B payload = 1467B) fits within [`UDP_BODY_LEN`].
pub const UPLOAD_SLICE_LEN: usize = 1456;

/// Number of slices tracked per [`RpcRequest::UploadSync`] `u64` bitmask window.
pub const UPLOAD_WINDOW_SLICES: usize = 64;

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

/// A request for the udp server.
///
/// To send a request to the server it must be serialized with postcard and sent
/// in a single udp packet at the address `<SERVER_IP>:25640`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UdpRequest<'a> {
    /// Used for server discovery.
    /// To be sent in broadcast over the lan.
    /// The server will reply with an empty packet.
    Discover,
    /// Sets the progress of the client for the current action.
    ActionProgress(usize, usize),
    /// Requests the given chunks to be broadcasted by the server.
    RequestChunks(Vec<ChunkHash>),
    /// Writes a slice of data (`<= UPLOAD_SLICE_LEN` bytes) into the client's upload buffer on the server.
    UploadSlice {
        id: u16,
        offset: u32,
        #[serde(borrow, with = "raw_bytes")]
        data: &'a [u8],
    },
    /// Request-response RPC with a sequence number echoed in the reply `(seq, response)`.
    Rpc { seq: u32, req: RpcRequest },
}

/// A request-response RPC carried inside [`UdpRequest::Rpc`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RpcRequest {
    /// Checks if the server contains the chunk in the database.
    /// The server will reply with a `bool`.
    HasChunk(ChunkHash),
    /// Checks if the server contains up to 32 chunks in the database.
    /// The server replies with a `u32` bitmask where bit `i` indicates chunk `i` is present.
    HasChunks(Vec<ChunkHash>),
    /// Asks the server for a slice of the serialized [`Image`] starting at `offset`.
    /// The server replies with `(total_len: u32, slice: Vec<u8>)`.
    GetImage { offset: u32 },
    /// Registers the client with the given info.
    /// The response is `()`.
    Register(RegistrationInfo, UnitStats),
    /// Returns a `u64` bitmask of which slices in `[base, base + count * UPLOAD_SLICE_LEN)`
    /// are present in the server's upload buffer.
    UploadSync { id: u16, base: u32, count: u8 },
    /// Commits the uploaded buffer as a compressed chunk of the given hash and byte size.
    /// The response is `()`.
    CommitChunk { hash: ChunkHash, size: u32 },
    /// Commits the uploaded buffer as the unit's [`Image`] of the given byte size.
    /// The response is `()`.
    CommitImage { size: u32 },
    /// Asks the server for the client action to run.
    /// The server will reply with an [`Action`].
    GetAction,
    /// Tells the server that the action is complete and can proceed to the next action.
    /// The response is `()`.
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

#[cfg(test)]
mod tests {
    use super::*;
    use core::fmt;
    use serde::{Deserialize, Serialize, de::Visitor, ser::Impossible};

    #[derive(Debug)]
    struct TestErr;
    impl fmt::Display for TestErr {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("test error")
        }
    }
    impl core::error::Error for TestErr {}
    impl serde::ser::Error for TestErr {
        fn custom<T: fmt::Display>(_msg: T) -> Self {
            Self
        }
    }
    impl serde::de::Error for TestErr {
        fn custom<T: fmt::Display>(_msg: T) -> Self {
            Self
        }
    }

    struct BytesSer;
    impl serde::Serializer for BytesSer {
        type Ok = Vec<u8>;
        type Error = TestErr;
        type SerializeSeq = Impossible<Vec<u8>, TestErr>;
        type SerializeTuple = Impossible<Vec<u8>, TestErr>;
        type SerializeTupleStruct = Impossible<Vec<u8>, TestErr>;
        type SerializeTupleVariant = Impossible<Vec<u8>, TestErr>;
        type SerializeMap = Impossible<Vec<u8>, TestErr>;
        type SerializeStruct = Impossible<Vec<u8>, TestErr>;
        type SerializeStructVariant = Impossible<Vec<u8>, TestErr>;
        fn serialize_bool(self, _: bool) -> Result<Vec<u8>, TestErr> {
            Err(TestErr)
        }
        fn serialize_i8(self, _: i8) -> Result<Vec<u8>, TestErr> {
            Err(TestErr)
        }
        fn serialize_i16(self, _: i16) -> Result<Vec<u8>, TestErr> {
            Err(TestErr)
        }
        fn serialize_i32(self, _: i32) -> Result<Vec<u8>, TestErr> {
            Err(TestErr)
        }
        fn serialize_i64(self, _: i64) -> Result<Vec<u8>, TestErr> {
            Err(TestErr)
        }
        fn serialize_u8(self, _: u8) -> Result<Vec<u8>, TestErr> {
            Err(TestErr)
        }
        fn serialize_u16(self, _: u16) -> Result<Vec<u8>, TestErr> {
            Err(TestErr)
        }
        fn serialize_u32(self, _: u32) -> Result<Vec<u8>, TestErr> {
            Err(TestErr)
        }
        fn serialize_u64(self, _: u64) -> Result<Vec<u8>, TestErr> {
            Err(TestErr)
        }
        fn serialize_f32(self, _: f32) -> Result<Vec<u8>, TestErr> {
            Err(TestErr)
        }
        fn serialize_f64(self, _: f64) -> Result<Vec<u8>, TestErr> {
            Err(TestErr)
        }
        fn serialize_char(self, _: char) -> Result<Vec<u8>, TestErr> {
            Err(TestErr)
        }
        fn serialize_str(self, _: &str) -> Result<Vec<u8>, TestErr> {
            Err(TestErr)
        }
        fn serialize_bytes(self, v: &[u8]) -> Result<Vec<u8>, TestErr> {
            Ok(v.to_vec())
        }
        fn serialize_none(self) -> Result<Vec<u8>, TestErr> {
            Err(TestErr)
        }
        fn serialize_some<T: ?Sized + Serialize>(self, _: &T) -> Result<Vec<u8>, TestErr> {
            Err(TestErr)
        }
        fn serialize_unit(self) -> Result<Vec<u8>, TestErr> {
            Err(TestErr)
        }
        fn serialize_unit_struct(self, _: &'static str) -> Result<Vec<u8>, TestErr> {
            Err(TestErr)
        }
        fn serialize_unit_variant(
            self,
            _: &'static str,
            _: u32,
            _: &'static str,
        ) -> Result<Vec<u8>, TestErr> {
            Err(TestErr)
        }
        fn serialize_newtype_struct<T: ?Sized + Serialize>(
            self,
            _: &'static str,
            value: &T,
        ) -> Result<Vec<u8>, TestErr> {
            value.serialize(self)
        }
        fn serialize_newtype_variant<T: ?Sized + Serialize>(
            self,
            _: &'static str,
            _: u32,
            _: &'static str,
            _: &T,
        ) -> Result<Vec<u8>, TestErr> {
            Err(TestErr)
        }
        fn serialize_seq(self, _: Option<usize>) -> Result<Self::SerializeSeq, TestErr> {
            Err(TestErr)
        }
        fn serialize_tuple(self, _: usize) -> Result<Self::SerializeTuple, TestErr> {
            Err(TestErr)
        }
        fn serialize_tuple_struct(
            self,
            _: &'static str,
            _: usize,
        ) -> Result<Self::SerializeTupleStruct, TestErr> {
            Err(TestErr)
        }
        fn serialize_tuple_variant(
            self,
            _: &'static str,
            _: u32,
            _: &'static str,
            _: usize,
        ) -> Result<Self::SerializeTupleVariant, TestErr> {
            Err(TestErr)
        }
        fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap, TestErr> {
            Err(TestErr)
        }
        fn serialize_struct(
            self,
            _: &'static str,
            _: usize,
        ) -> Result<Self::SerializeStruct, TestErr> {
            Err(TestErr)
        }
        fn serialize_struct_variant(
            self,
            _: &'static str,
            _: u32,
            _: &'static str,
            _: usize,
        ) -> Result<Self::SerializeStructVariant, TestErr> {
            Err(TestErr)
        }
    }

    struct BytesDe<'de>(&'de [u8]);
    impl<'de> serde::Deserializer<'de> for BytesDe<'de> {
        type Error = TestErr;
        fn deserialize_bytes<V: Visitor<'de>>(self, v: V) -> Result<V::Value, TestErr> {
            v.visit_borrowed_bytes(self.0)
        }
        fn deserialize_newtype_struct<V: Visitor<'de>>(
            self,
            _: &'static str,
            v: V,
        ) -> Result<V::Value, TestErr> {
            v.visit_newtype_struct(self)
        }
        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string byte_buf
            option unit unit_struct seq tuple tuple_struct map
            struct enum identifier ignored_any
        }
        fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, TestErr> {
            Err(TestErr)
        }
    }

    #[test]
    fn raw_bytes_borrows_zero_copy_and_upload_bounds() {
        const {
            assert!(UPLOAD_WINDOW_SLICES == 64);
            assert!(1 + 3 + 5 + 2 + UPLOAD_SLICE_LEN <= UDP_BODY_LEN);
        }

        #[derive(Serialize, Deserialize)]
        struct Wrapper<'a>(#[serde(borrow, with = "raw_bytes")] &'a [u8]);

        let payload = [0xABu8; UPLOAD_SLICE_LEN];
        let encoded = Wrapper(&payload).serialize(BytesSer).unwrap();
        let decoded = Wrapper::deserialize(BytesDe(&encoded)).unwrap().0;
        assert_eq!(decoded, &payload);
        assert!(encoded.as_ptr_range().contains(&decoded.as_ptr()));
    }
}
