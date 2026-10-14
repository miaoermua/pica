use crate::app::CliResult;

pub mod apk;
pub mod opkg;

pub use apk::ApkBackend;
pub use opkg::OpkgBackend;

/// Backend identity of a package. `None` is not "no value": it is the Pica
/// native backend, which installs `root/` itself and never delegates to an
/// external package manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageManagerKind {
  Opkg,
  Apk,
  None,
}

impl PackageManagerKind {
  pub fn parse(value: &str) -> Option<Self> {
    match value.trim().to_ascii_lowercase().as_str() {
      "opkg" => Some(Self::Opkg),
      "apk" => Some(Self::Apk),
      "none" => Some(Self::None),
      _ => None,
    }
  }

  pub fn as_str(self) -> &'static str {
    match self {
      Self::Opkg => "opkg",
      Self::Apk => "apk",
      Self::None => "none",
    }
  }

  /// Native backends own their dependency resolution and are probed on the
  /// host; `none` has no external command to look for.
  #[must_use]
  pub fn is_native(self) -> bool {
    matches!(self, Self::Opkg | Self::Apk)
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
  fn architectures(&self) -> Vec<String>;

  /// Installed packages with their raw backend version strings.
  fn installed(&self) -> Vec<(String, String)>;

  /// Dependency names declared by `name`, without version constraints.
  fn depends(&self, name: &str) -> Vec<String>;

  /// Name of the installed package owning `path`, when the backend can tell.
  fn owns_file(&self, path: &str) -> Option<String>;

  fn snapshot_installed(&self) -> Vec<String> {
    let mut packages: Vec<String> = self.installed().into_iter().map(|(name, _)| name).collect();
    packages.sort();
    packages.dedup();
    packages
  }
}

pub fn package_manager_for(kind: PackageManagerKind) -> Option<Box<dyn PackageManager>> {
  match kind {
    PackageManagerKind::Opkg if crate::system::has_command("opkg") => Some(Box::new(OpkgBackend)),
    PackageManagerKind::Apk if crate::system::has_command("apk") => Some(Box::new(ApkBackend)),
    _ => None,
  }
}

/// Auto-detection order used when neither the CLI, the package manifest nor the
/// configuration file pins a backend: `apk` before `opkg`, then Pica-native
/// `none` when the host has neither.
pub fn detect_kind() -> PackageManagerKind {
  if crate::system::has_command("apk") {
    PackageManagerKind::Apk
  } else if crate::system::has_command("opkg") {
    PackageManagerKind::Opkg
  } else {
    PackageManagerKind::None
  }
}

#[cfg(test)]
mod tests {
  use super::PackageManagerKind;
  use pretty_assertions::assert_eq;

  #[test]
  fn parses_supported_package_manager_kinds() {
    assert_eq!(PackageManagerKind::parse("opkg"), Some(PackageManagerKind::Opkg));
    assert_eq!(PackageManagerKind::parse(" APK "), Some(PackageManagerKind::Apk));
    assert_eq!(PackageManagerKind::parse("None"), Some(PackageManagerKind::None));
    assert_eq!(PackageManagerKind::parse("pica"), None);
    assert_eq!(PackageManagerKind::parse(""), None);
  }

  #[test]
  fn kind_names_round_trip_through_parse() {
    for kind in [PackageManagerKind::Opkg, PackageManagerKind::Apk, PackageManagerKind::None] {
      assert_eq!(PackageManagerKind::parse(kind.as_str()), Some(kind));
    }
  }

  #[test]
  fn only_opkg_and_apk_are_native_backends() {
    assert!(PackageManagerKind::Opkg.is_native());
    assert!(PackageManagerKind::Apk.is_native());
    assert!(!PackageManagerKind::None.is_native());
  }
}
