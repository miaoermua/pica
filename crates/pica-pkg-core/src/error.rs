use std::fmt;
use std::io;

/// The stable identifier of a failure, as `--json-errors` reports it.
///
/// The set is closed so a caller can match on it, and each value is part of the
/// output contract: docs/standard.md lists them, and a test keeps the two equal.
/// `E_OPKG_INSTALL` and `E_OPKG_REMOVE` are the names opkg failures had before
/// the neutral `E_PKGMGR_*` pair existed, and opkg still reports them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
  Runtime,
  ArgInvalid,
  Io,
  JsonInvalid,
  DbInvalid,
  IndexInvalid,
  IndexUnavailable,
  ConfigInvalid,
  PolicyInvalid,
  RepoInvalid,
  MissingCommand,
  LockBusy,
  OpkgInstall,
  OpkgRemove,
  NoSpace,
  ManifestInvalid,
  PackageInvalid,
  PlatformUnsupported,
  VersionIncompatible,
  IntegrityInvalid,
  NotInteractive,
  PkgmgrInstall,
  PkgmgrRemove,
  Cancelled,
}

impl ErrorCode {
  /// Every code, in declaration order.
  pub const ALL: &'static [Self] = &[
    Self::Runtime,
    Self::ArgInvalid,
    Self::Io,
    Self::JsonInvalid,
    Self::DbInvalid,
    Self::IndexInvalid,
    Self::IndexUnavailable,
    Self::ConfigInvalid,
    Self::PolicyInvalid,
    Self::RepoInvalid,
    Self::MissingCommand,
    Self::LockBusy,
    Self::OpkgInstall,
    Self::OpkgRemove,
    Self::NoSpace,
    Self::ManifestInvalid,
    Self::PackageInvalid,
    Self::PlatformUnsupported,
    Self::VersionIncompatible,
    Self::IntegrityInvalid,
    Self::NotInteractive,
    Self::PkgmgrInstall,
    Self::PkgmgrRemove,
    Self::Cancelled,
  ];

  #[must_use]
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Runtime => "E_RUNTIME",
      Self::ArgInvalid => "E_ARG_INVALID",
      Self::Io => "E_IO",
      Self::JsonInvalid => "E_JSON_INVALID",
      Self::DbInvalid => "E_DB_INVALID",
      Self::IndexInvalid => "E_INDEX_INVALID",
      Self::IndexUnavailable => "E_INDEX_UNAVAILABLE",
      Self::ConfigInvalid => "E_CONFIG_INVALID",
      Self::PolicyInvalid => "E_POLICY_INVALID",
      Self::RepoInvalid => "E_REPO_INVALID",
      Self::MissingCommand => "E_MISSING_COMMAND",
      Self::LockBusy => "E_LOCK_BUSY",
      Self::OpkgInstall => "E_OPKG_INSTALL",
      Self::OpkgRemove => "E_OPKG_REMOVE",
      Self::NoSpace => "E_NO_SPACE",
      Self::ManifestInvalid => "E_MANIFEST_INVALID",
      Self::PackageInvalid => "E_PACKAGE_INVALID",
      Self::PlatformUnsupported => "E_PLATFORM_UNSUPPORTED",
      Self::VersionIncompatible => "E_VERSION_INCOMPATIBLE",
      Self::IntegrityInvalid => "E_INTEGRITY_INVALID",
      Self::NotInteractive => "E_NOT_INTERACTIVE",
      Self::PkgmgrInstall => "E_PKGMGR_INSTALL",
      Self::PkgmgrRemove => "E_PKGMGR_REMOVE",
      Self::Cancelled => "E_CANCELLED",
    }
  }
}

impl fmt::Display for ErrorCode {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str(self.as_str())
  }
}

#[cfg(test)]
mod tests {
  use super::ErrorCode;

  #[test]
  fn every_code_is_documented() {
    let standard = include_str!("../../../docs/standard.md");
    for code in ErrorCode::ALL {
      assert!(standard.contains(&format!("| `{code}` |")), "{code} is missing from standard.md");
    }
  }

  #[test]
  fn every_code_spells_its_own_name() {
    for code in ErrorCode::ALL {
      assert!(code.as_str().starts_with("E_"), "{code:?}");
    }
  }
}

#[derive(Debug, thiserror::Error)]
#[allow(clippy::module_name_repetitions)]
pub enum PicaError {
  #[error("{0}")]
  Message(String),
  /// A failure that knows what kind it is, so the CLI does not have to report
  /// it as a runtime error.
  #[error("{message}")]
  Coded { code: ErrorCode, message: String },
  #[error("{0}")]
  Io(#[from] io::Error),
  #[error("{0}")]
  Json(#[from] serde_json::Error),
}

impl PicaError {
  pub fn msg(value: impl Into<String>) -> Self {
    Self::Message(value.into())
  }

  pub fn coded(code: ErrorCode, message: impl Into<String>) -> Self {
    Self::Coded { code, message: message.into() }
  }
}

pub type PicaResult<T> = Result<T, PicaError>;
