use alloc::{borrow::ToOwned, string::String};
use core::fmt::{Display, Formatter};
use derive_more::From;
use smoltcp::socket::{
    tcp::{self, ConnectError, RecvError},
    udp::{self, BindError},
};

pub type Result<T, E = Error> = core::result::Result<T, E>;

#[derive(Debug, From)]
pub enum Error {
    Connect(#[from] ConnectError),
    TcpSend(#[from] tcp::SendError),
    UdpSend(#[from] udp::SendError),
    Recv(#[from] RecvError),
    Bind(#[from] BindError),
    Postcard(#[from] postcard::Error),
    Uefi(#[from] uefi::Error),
    Generic(String),
}

impl Error {
    pub fn msg(s: &str) -> Error {
        Error::Generic(s.to_owned())
    }
}

impl Display for Error {
    fn fmt(&self, fmt: &mut Formatter<'_>) -> Result<(), core::fmt::Error> {
        write!(fmt, "{self:?}")
    }
}
