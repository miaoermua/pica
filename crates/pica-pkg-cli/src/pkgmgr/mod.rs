use crate::app::CliResult;

pub mod apk;
pub mod opkg;

pub use apk::ApkBackend;
pub use opkg::OpkgBackend;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageManagerKind {
  Opkg,
  Apk,
}

impl PackageManagerKind {
  pub fn parse(value: &str) -> Option<Self> {
    match value.trim().to_ascii_lowercase().as_str() {
      "opkg" => Some(Self::Opkg),
      "apk" => Some(Self::Apk),
      _ => None,
    }
  }

  pub fn as_str(self) -> &'static str {
    match self {
      Self::Opkg => "opkg",
      Self::Apk => "apk",
    }
  }
}

pub trait PackageManager {
  fn kind(&self) -> PackageManagerKind;
  fn update_index(&self);
  fn package_exists(&self, name: &str) -> bool;
  fn is_installed(&self, name: &str) -> bool;
  fn installed_version(&self, name: &str) -> Option<String>;
  fn install(&self, label: &str, target: &str) -> CliResult<()>;
  fn remove(&self, target: &str) -> CliResult<()>;
  fn snapshot_installed(&self) -> Vec<String>;
  fn architectures(&self) -> Vec<String>;
}

pub fn package_manager_for(kind: PackageManagerKind) -> Option<Box<dyn PackageManager>> {
  match kind {
    PackageManagerKind::Opkg if crate::system::has_command("opkg") => Some(Box::new(OpkgBackend)),
    PackageManagerKind::Apk if crate::system::has_command("apk") => Some(Box::new(ApkBackend)),
    _ => None,
  }
}

pub fn detect_package_manager() -> Option<Box<dyn PackageManager>> {
  package_manager_for(PackageManagerKind::Apk).or_else(|| package_manager_for(PackageManagerKind::Opkg))
}

#[cfg(test)]
mod tests {
  use super::PackageManagerKind;

  #[test]
  fn parses_supported_package_manager_kinds() {
    assert_eq!(PackageManagerKind::parse("opkg"), Some(PackageManagerKind::Opkg));
    assert_eq!(PackageManagerKind::parse(" APK "), Some(PackageManagerKind::Apk));
    assert_eq!(PackageManagerKind::parse("none"), None);
    assert_eq!(PackageManagerKind::Apk.as_str(), "apk");
  }
}
