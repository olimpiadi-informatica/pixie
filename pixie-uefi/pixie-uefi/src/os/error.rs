use alloc::{borrow::ToOwned, string::String};
use smoltcp::socket::{
    tcp::{self, ConnectError, RecvError},
    udp::{self, BindError},
};

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Connect(ConnectError),
    TcpSend(tcp::SendError),
    UdpSend(udp::SendError),
    Recv(RecvError),
    Bind(BindError),
    Serde(serde_json::Error),
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

impl From<serde_json::Error> for Error {
    fn from(c: serde_json::Error) -> Self {
        Error::Serde(c)
    }
}
