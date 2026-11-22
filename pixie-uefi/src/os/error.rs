use alloc::borrow::ToOwned;
use alloc::string::String;
use core::fmt::{Display, Formatter};

use gpt_disk_io::gpt_disk_types;

pub type Result<T, E = Error> = core::result::Result<T, E>;

#[derive(Debug)]
pub struct Error(pub String);

impl Error {
    pub fn msg(s: &str) -> Error {
        Self(s.to_owned())
    }
}

macro_rules! err {
    ($ty: ty) => {
        impl From<$ty> for Error {
            fn from(value: $ty) -> Self {
                Self(format!("{}: {value}", stringify!($ty)))
            }
        }
    };
}

err!(uefi::Error);
err!(postcard::Error);
err!(lz4_flex::block::DecompressError);
err!(gpt_disk_io::DiskError<Error>);
err!(gpt_disk_types::GptPartitionEntrySizeError);

impl Display for Error {
    fn fmt(&self, fmt: &mut Formatter<'_>) -> Result<(), core::fmt::Error> {
        write!(fmt, "{self:?}")
    }
}
