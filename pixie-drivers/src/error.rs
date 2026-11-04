use alloc::borrow::Cow;
use alloc::string::String;
use core::fmt::{Display, Formatter};

pub type Result<T, E = Error> = core::result::Result<T, E>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub Cow<'static, str>);

impl Error {
    pub const fn msg(s: &'static str) -> Self {
        Self(Cow::Borrowed(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&'static str> for Error {
    fn from(s: &'static str) -> Self {
        Self::msg(s)
    }
}

impl From<String> for Error {
    fn from(s: String) -> Self {
        Self(Cow::Owned(s))
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}
