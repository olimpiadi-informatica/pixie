use alloc::borrow::ToOwned;
use alloc::string::String;
use core::fmt::{Display, Formatter};

pub type Result<T, E = Error> = core::result::Result<T, E>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl Error {
    pub fn msg(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<&str> for Error {
    fn from(s: &str) -> Self {
        Self::msg(s)
    }
}

impl From<String> for Error {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}
