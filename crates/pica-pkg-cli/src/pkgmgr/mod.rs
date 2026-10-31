use crate::app::CliResult;
use crate::i18n::Key;
use crate::i18n::tr;

pub mod apk;
pub(crate) mod apk_repositories;
pub(crate) mod index;
pub mod opkg;

pub use apk::ApkBackend;
pub use opkg::OpkgBackend;

/// Backend identity of a package. The three values answer "who owns these
/// files": `opkg` and `apk` hand that to a system package manager, while `pica`
/// keeps it and unpacks any `pkgs/` container itself.
///
/// The one capability this enum reports is [`Self::requires_system_binary`]:
/// whether the backend has a system package manager to ask. That is a different
/// question from who installs declared dependencies — `pica` owns files but has
/// nobody to ask, while `opkg`/`apk` pass the request on. Dependency resolution
/// is not a backend capability in pica; it is a non-goal.
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
}

/// One version of one package that a feed offers, with the sizes the feed
/// recorded for it.
///
/// The two sizes are different figures and neither is guaranteed: `download` is
/// the size of the package file the installer fetches, `installed` is what the
/// unpacked package takes on the device. A backend that reports only one of
/// them leaves the other `None` rather than filling it with the figure it has —
/// an estimate that quietly swapped the two would be wrong in the direction the
/// user is deciding about (bytes over the air).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedCandidate {
  /// The version as the backend prints it, ordered by
  /// [`pica_pkg_core::version::pkgver_cmp`] so the newest can be picked.
  pub version: String,
  /// Bytes to download. `opkg`'s `Size`, `apk`'s `file-size`.
  pub download: Option<u64>,
  /// Bytes installed. `opkg`'s `Installed-Size`, `apk`'s `installed-size`.
  pub installed: Option<u64>,
}

pub trait PackageManager {
  fn kind(&self) -> PackageManagerKind;

  /// Makes the dependency index of this host's configured feeds readable,
  /// fetching it only when it is not already there.
  ///
  /// `Err` when a configured feed has no index and the backend's own refresh
  /// did not produce one. That is the state in which every "which packages
  /// exist" question has no answer, so callers that would otherwise read the
  /// backend's silence as "not available" must stop instead.
  fn update_index(&self) -> CliResult<()>;

  /// Whether `name` is a package of one of this host's configured feeds.
  ///
  /// `Ok(false)` is the answer "the feeds do not have it". `Err` is the answer
  /// "the feeds cannot be read", which no caller may treat as the first: a
  /// backend whose index was never fetched reports every package as missing.
  fn package_exists(&self, name: &str) -> CliResult<bool>;

  /// Every version of `name` the feeds offer, oldest to newest, each with the
  /// sizes that feed wrote down for it.
  ///
  /// `Ok(empty)` is the answer "the feeds do not have it". `Err` is the answer
  /// "the feeds cannot be read", and the two may not be confused: an index that
  /// was never fetched lists nothing, which is not the same as a package no feed
  /// carries. Sizes are `None` when the backend does not report them — asking for
  /// them costs nothing extra here, because they come from the same index entry.
  fn feed_candidates(&self, name: &str) -> CliResult<Vec<FeedCandidate>>;
  fn is_installed(&self, name: &str) -> bool;
  fn installed_version(&self, name: &str) -> Option<String>;
  /// Installs `targets` — package names, or the paths of local package files —
  /// in one call to the backend.
  ///
  /// `label` names the group of the request for messages ("base", "app",
  /// "kmod", "packaged"). One call carries however many targets the request
  /// has, because a request can be the whole set of containers an artifact
  /// ships: the containers of one application depend on each other, and a
  /// backend can only satisfy such a dependency from packages that arrive in
  /// the same request. Installing them one at a time puts the order — some
  /// order pica chose, alphabetical or otherwise — in charge of resolution,
  /// and no order pica can pick is a topological one.
  fn install(&self, label: &str, targets: &[String]) -> CliResult<()>;
  fn remove(&self, target: &str) -> CliResult<()>;
  fn architectures(&self) -> Vec<String>;

  /// Installed packages with their raw backend version strings.
  ///
  /// `Err` means the list could not be read. Callers must not read that as an
  /// empty system: the install's rollback record is built by diffing this list,
  /// and "nothing installed" is exactly what would make a transaction forget
  /// the packages it added.
  fn installed(&self) -> CliResult<Vec<(String, String)>>;

  /// Dependency names declared by `name`, without version constraints.
  fn depends(&self, name: &str) -> Vec<String>;

  /// Name of the installed package owning `path`, when the backend can tell.
  fn owns_file(&self, path: &str) -> Option<String>;

  fn snapshot_installed(&self) -> CliResult<Vec<String>> {
    let mut packages: Vec<String> = self.installed()?.into_iter().map(|(name, _)| name).collect();
    packages.sort();
    packages.dedup();
    Ok(packages)
  }
}

/// How a set of install targets reads in a message: the target itself when the
/// request held one, and a bounded list when the backend was handed a whole set.
pub(crate) fn describe_targets(targets: &[String]) -> String {
  /// Targets listed before the message switches to a count.
  const SHOWN: usize = 3;

  match targets {
    [] => "nothing".to_string(),
    [one] => one.clone(),
    many => {
      let shown: Vec<&str> = many.iter().take(SHOWN).map(String::as_str).collect();
      let rest = many.len() - shown.len();
      if rest == 0 {
        tr(Key::PackagesList, &[&many.len(), &shown.join(", ")])
      } else {
        tr(Key::PackagesAndMore, &[&many.len(), &shown.join(", "), &rest])
      }
    }
  }
}

/// The backend for `kind` as the configuration file describes it.
///
/// `apk_force_non_repository` is the only setting a backend takes: it decides
/// whether a local `.apk` is installed with the flag apk-tools 3.x wants for a
/// package no repository provides. Everything that installs goes through here;
/// [`package_manager_for`] is for callers that only ask questions.
pub fn package_manager_with(
  kind: PackageManagerKind,
  apk_force_non_repository: bool,
) -> Option<Box<dyn PackageManager>> {
  match kind {
    PackageManagerKind::Opkg if crate::system::has_command("opkg") => Some(Box::new(OpkgBackend)),
    PackageManagerKind::Apk if crate::system::has_command("apk") => {
      Some(Box::new(ApkBackend::new(apk_force_non_repository)))
    }
    _ => None,
  }
}

/// The backend for `kind`, with the local-install policy left at its default.
///
/// Probing a host — its architectures, its luci variant — asks the backend
/// questions and installs nothing, so it does not need the setting.
pub fn package_manager_for(kind: PackageManagerKind) -> Option<Box<dyn PackageManager>> {
  package_manager_with(kind, true)
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

/// Every backend this host can actually use, in detection order.
///
/// This is the host half of the intersection a source's declared backends are
/// matched against, so it must not be wider than what can really run here:
/// `apk` and `opkg` only when their binary is on `PATH`, and `pica` always,
/// because the native backend needs no external command.
#[must_use]
pub fn available_kinds() -> Vec<PackageManagerKind> {
  let mut kinds = Vec::new();
  if crate::system::has_command("apk") {
    kinds.push(PackageManagerKind::Apk);
  }
  if crate::system::has_command("opkg") {
    kinds.push(PackageManagerKind::Opkg);
  }
  kinds.push(PackageManagerKind::Pica);
  kinds
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
}
