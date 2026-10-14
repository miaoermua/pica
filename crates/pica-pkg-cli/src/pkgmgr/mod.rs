use crate::app::CliResult;

pub mod apk;
pub mod opkg;

pub use apk::ApkBackend;
pub use opkg::OpkgBackend;

/// Backend identity of a package. The three values answer "who owns these
/// files": `opkg` and `apk` hand that to a system package manager, while `pica`
/// keeps it and unpacks any `pkgs/` container itself.
///
/// The two capabilities a backend may have are deliberately separate questions
/// ([`Self::requires_system_binary`] and [`Self::can_resolve_dependencies`]),
/// because `pica` currently owns its files without being able to solve
/// dependencies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageManagerKind {
  Opkg,
  Apk,
  Pica,
}

impl PackageManagerKind {
  pub fn parse(value: &str) -> Option<Self> {
    match value.trim().to_ascii_lowercase().as_str() {
      "opkg" => Some(Self::Opkg),
      "apk" => Some(Self::Apk),
      "pica" => Some(Self::Pica),
      _ => None,
    }
  }

  /// Parses a backend name the way an existing database row may have written it.
  ///
  /// `none` is the name this backend was recorded under before it became `pica`,
  /// and rows from those releases are still on disk: the `pkgmgr`, `source` and
  /// `pkgfile` fields all carried it. Reading such a row through [`Self::parse`]
  /// would find nothing and let the ladder fall through to `opkg`, which would
  /// then be handed a payload it never installed.
  ///
  /// Only for reading records. User input keeps using [`Self::parse`], so
  /// `--pkgmgr none` stays an error instead of quietly meaning `pica`.
  #[must_use]
  pub fn parse_legacy(value: &str) -> Option<Self> {
    match value.trim().to_ascii_lowercase().as_str() {
      "none" => Some(Self::Pica),
      other => Self::parse(other),
    }
  }

  pub fn as_str(self) -> &'static str {
    match self {
      Self::Opkg => "opkg",
      Self::Apk => "apk",
      Self::Pica => "pica",
    }
  }

  /// Whether the backend shells out to a system package manager, which is also
  /// what decides if the host must have that command installed and if the
  /// backend's database can be queried for ownership and dependencies.
  #[must_use]
  pub fn requires_system_binary(self) -> bool {
    matches!(self, Self::Opkg | Self::Apk)
  }

  /// Whether the backend can turn declared dependencies into installed ones.
  ///
  /// `opkg` and `apk` delegate that to the system, which is why they answer
  /// `false` here: they *pass the request on* rather than resolving it. `pica`
  /// unpacks its containers but has no solver yet, so this is `false` for all
  /// three today. It exists so the "can anyone satisfy these dependencies" gate
  /// has a single place to widen once the solver milestone lands.
  // The answer does not depend on which backend is asked yet: every backend is
  // `false` until a solver exists.
  #[allow(clippy::unused_self)]
  #[must_use]
  pub fn can_resolve_dependencies(self) -> bool {
    false
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
/// configuration file pins a backend: `apk` before `opkg`, then the Pica-native
/// backend when the host has neither.
pub fn detect_kind() -> PackageManagerKind {
  if crate::system::has_command("apk") {
    PackageManagerKind::Apk
  } else if crate::system::has_command("opkg") {
    PackageManagerKind::Opkg
  } else {
    PackageManagerKind::Pica
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
    assert_eq!(PackageManagerKind::parse("Pica"), Some(PackageManagerKind::Pica));
    assert_eq!(PackageManagerKind::parse(""), None);
  }

  #[test]
  fn the_retired_none_spelling_is_no_longer_accepted() {
    assert_eq!(PackageManagerKind::parse("none"), None, "`none` was replaced by `pica`");
    assert_eq!(PackageManagerKind::parse("NONE"), None);
  }

  #[test]
  fn reading_a_row_still_understands_the_retired_spelling() {
    // Rejecting `none` as user input is not the same as failing to read it out
    // of a record an older release wrote.
    assert_eq!(PackageManagerKind::parse_legacy("none"), Some(PackageManagerKind::Pica));
    assert_eq!(PackageManagerKind::parse_legacy(" None "), Some(PackageManagerKind::Pica));
    assert_eq!(PackageManagerKind::parse_legacy("apk"), Some(PackageManagerKind::Apk));
    assert_eq!(PackageManagerKind::parse_legacy("/tmp/demo.pkg.tar.gz"), None);

    for kind in [PackageManagerKind::Opkg, PackageManagerKind::Apk, PackageManagerKind::Pica] {
      assert_eq!(PackageManagerKind::parse_legacy(kind.as_str()), Some(kind));
    }
  }

  #[test]
  fn kind_names_round_trip_through_parse() {
    for kind in [PackageManagerKind::Opkg, PackageManagerKind::Apk, PackageManagerKind::Pica] {
      assert_eq!(PackageManagerKind::parse(kind.as_str()), Some(kind));
    }
  }

  #[test]
  fn only_opkg_and_apk_need_a_system_binary() {
    assert!(PackageManagerKind::Opkg.requires_system_binary());
    assert!(PackageManagerKind::Apk.requires_system_binary());
    assert!(!PackageManagerKind::Pica.requires_system_binary());
  }

  #[test]
  fn no_backend_resolves_dependencies_yet() {
    // `opkg`/`apk` forward the list to the system instead of solving it, and
    // `pica` has no solver; the three answers are the same on purpose.
    for kind in [PackageManagerKind::Opkg, PackageManagerKind::Apk, PackageManagerKind::Pica] {
      assert!(!kind.can_resolve_dependencies(), "{kind:?}");
    }
  }
}
