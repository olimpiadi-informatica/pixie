use alloc::{borrow::ToOwned, string::String};
use smoltcp::socket::tcp::{ConnectError, RecvError, SendError};

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    Connect(ConnectError),
    Send(SendError),
    Recv(RecvError),
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

impl From<SendError> for Error {
    fn from(c: SendError) -> Self {
        Error::Send(c)
    }
}

impl From<RecvError> for Error {
    fn from(c: RecvError) -> Self {
        Error::Recv(c)
    }
}
