use std::io;

#[derive(Debug, thiserror::Error)]
#[allow(clippy::module_name_repetitions)]
pub enum PicaError {
  #[error("{0}")]
  Message(String),
  #[error("{0}")]
  Io(#[from] io::Error),
  #[error("{0}")]
  Json(#[from] serde_json::Error),
}

impl PicaError {
  pub fn msg(value: impl Into<String>) -> Self {
    Self::Message(value.into())
  }
}

pub type PicaResult<T> = Result<T, PicaError>;
