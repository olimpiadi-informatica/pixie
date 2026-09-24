use alloc::string::String;
use core::fmt::{Display, Formatter};

use gpt_disk_io::gpt_disk_types;

pub type Result<T, E = Error> = core::result::Result<T, E>;

#[derive(Debug)]
pub struct Error(pub String);

impl Error {
    pub fn msg(s: impl core::fmt::Display) -> Error {
        use alloc::string::ToString;
        Self(s.to_string())
    }
}

impl From<String> for Error {
    fn from(value: String) -> Self {
        Self(value)
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
err!(smoltcp::socket::tcp::ConnectError);
err!(smoltcp::socket::tcp::RecvError);
err!(smoltcp::socket::tcp::SendError);
err!(smoltcp::socket::udp::BindError);
err!(smoltcp::socket::udp::SendError);
err!(postcard::Error);
err!(lz4_flex::block::DecompressError);
err!(gpt_disk_io::DiskError<Error>);
err!(gpt_disk_types::GptPartitionEntrySizeError);

impl Display for Error {
    fn fmt(&self, fmt: &mut Formatter<'_>) -> Result<(), core::fmt::Error> {
        write!(fmt, "{self:?}")
    }
}
