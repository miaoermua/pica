use std::fmt::{Display, Formatter};
use std::io;

#[derive(Debug)]
pub enum PicaError {
    Message(String),
    Io(io::Error),
    Json(serde_json::Error),
}

impl PicaError {
    pub fn msg(value: impl Into<String>) -> Self {
        Self::Message(value.into())
    }
}

impl Display for PicaError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message(value) => write!(f, "{value}"),
            Self::Io(err) => write!(f, "{err}"),
            Self::Json(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for PicaError {}

impl From<io::Error> for PicaError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for PicaError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

pub type PicaResult<T> = Result<T, PicaError>;
