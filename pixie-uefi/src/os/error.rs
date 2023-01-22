use core::fmt::{Display, Formatter};

use alloc::{borrow::ToOwned, string::String};
use smoltcp::socket::{
    tcp::{self, ConnectError, RecvError},
    udp::{self, BindError},
};

pub type Result<T, E = Error> = core::result::Result<T, E>;

#[derive(Debug)]
pub enum Error {
    Connect(ConnectError),
    TcpSend(tcp::SendError),
    UdpSend(udp::SendError),
    Recv(RecvError),
    Bind(BindError),
    Postcard(postcard::Error),
    Uefi(uefi::Error),
    Generic(String),
}

impl Error {
    pub fn msg(s: &str) -> Error {
        Error::Generic(s.to_owned())
    }
}

impl From<ConnectError> for Error {
    fn from(c: ConnectError) -> Self {
        Error::Connect(c)
    }
}

impl From<tcp::SendError> for Error {
    fn from(c: tcp::SendError) -> Self {
        Error::TcpSend(c)
    }
}

impl From<udp::SendError> for Error {
    fn from(c: udp::SendError) -> Self {
        Error::UdpSend(c)
    }
}

impl From<RecvError> for Error {
    fn from(c: RecvError) -> Self {
        Error::Recv(c)
    }
}

impl From<BindError> for Error {
    fn from(c: BindError) -> Self {
        Error::Bind(c)
    }
}

impl From<postcard::Error> for Error {
    fn from(c: postcard::Error) -> Self {
        Error::Postcard(c)
    }
}

impl From<uefi::Error> for Error {
    fn from(c: uefi::Error) -> Self {
        Error::Uefi(c)
    }
}

impl Display for Error {
    fn fmt(&self, fmt: &mut Formatter<'_>) -> Result<(), core::fmt::Error> {
        write!(fmt, "{:?}", self)
    }
}
