//! Semantics of the `source` and `pkgmgr` fields of an installed database entry.
//!
//! The two fields answer two different questions, and conflating them makes
//! removal act on the wrong package database:
//!
//! - `pkgmgr` — *which backend owns the installed files*: `opkg`, `apk` or
//!   `pica`. Always present, never empty.
//! - `source` — *how Pica obtained the package*: a repository index, a local
//!   artifact, an artifact URL, or the backend's own feeds.
//!
//! Older releases stored the backend name in `source` for feed installs (only
//! `opkg` existed then) and left `pkgmgr` to an install-time default of `opkg`.
//! Rows in that shape are migrated on read, so a database written by any
//! release keeps working without being rewritten.

use crate::pkgmgr::PackageManagerKind;
use pica_pkg_core::layout::PKG_EXTENSION;
use pica_pkg_core::model::db::DbEntry;

// A Pica artifact is recognised by `PKG_EXTENSION`. Feed installs record the
// backend name in `pkgfile` instead, which is what makes that column a usable
// discriminator.

/// How Pica obtained an installed package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntrySource {
  /// Resolved through a Pica repository index.
  Repo,
  /// Installed from a local `.pkg.tar.gz`.
  Local,
  /// Installed from a `.pkg.tar.gz` URL.
  Url,
  /// Installed straight from the backend's feeds: no Pica artifact exists, so
  /// only the backend can remove it.
  Feed,
}

impl EntrySource {
  #[must_use]
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Repo => "pica",
      Self::Local => "local",
      Self::Url => "url",
      Self::Feed => "feed",
    }
  }

  /// Parses a stored value.
  ///
  /// Legacy rows recorded the backend name instead of `feed`, so any backend
  /// name means "came from the feeds".
  #[must_use]
  pub fn parse(value: &str) -> Option<Self> {
    match value.trim().to_ascii_lowercase().as_str() {
      "pica" => Some(Self::Repo),
      "local" => Some(Self::Local),
      "url" => Some(Self::Url),
      "feed" => Some(Self::Feed),
      other if PackageManagerKind::parse(other).is_some() => Some(Self::Feed),
      _ => None,
    }
  }

  /// `true` when the package can be re-resolved from a repository index.
  ///
  /// A local file or URL cannot: there is no index entry to compare versions
  /// against, so such packages are left out of upgrades.
  #[must_use]
  pub fn is_repo_resolvable(self) -> bool {
    self == Self::Repo
  }
}

/// Where the backend recorded for an entry came from, so callers can say so
/// instead of silently guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PkgmgrOrigin {
  /// The entry records `pkgmgr` itself.
  Recorded,
  /// Only the legacy `source` field named the backend.
  LegacySource,
  /// Only the recorded `pkgfile` named the backend.
  LegacyPkgfile,
  /// Nothing identified a backend; a Pica-native default applies.
  Default,
}

impl PkgmgrOrigin {
  /// Describes the origin for user-facing output, in the run's language.
  #[must_use]
  pub fn text(self) -> String {
    let key = match self {
      Self::Recorded => crate::i18n::Key::OriginRecorded,
      Self::LegacySource => crate::i18n::Key::OriginLegacySource,
      Self::LegacyPkgfile => crate::i18n::Key::OriginLegacyPkgfile,
      Self::Default => crate::i18n::Key::OriginDefault,
    };
    crate::i18n::tr(key, &[])
  }
}

/// Backend that owns an installed entry's files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryBackend {
  pub kind: PackageManagerKind,
  pub origin: PkgmgrOrigin,
}

impl EntryBackend {
  /// `true` when the backend had to be inferred rather than read from the
  /// entry, which is worth telling the user before acting on it.
  #[must_use]
  pub fn is_inferred(self) -> bool {
    self.origin != PkgmgrOrigin::Recorded
  }
}

/// Resolves the backend that owns an entry.
///
/// The ladder only falls through when a field is missing or unreadable, so a
/// row written by any release resolves to what that release actually did:
///
/// 1. `pkgmgr` — every install record written since the field existed.
/// 2. legacy `source` holding a backend name — `-So` rows of the `opkg` era.
/// 3. legacy `pkgfile` holding a backend name — same rows with `source` lost.
/// 4. a recorded artifact path — Pica deployed the payload itself, so no
///    backend may be asked to remove anything.
/// 5. otherwise `opkg`, the backend every earlier Pica release assumed.
///
/// Each field is read with [`PackageManagerKind::parse_legacy`], because a row
/// may name the backend with the spelling it had before it became `pica`.
#[must_use]
pub fn entry_backend(entry: &DbEntry) -> EntryBackend {
  if let Some(kind) = PackageManagerKind::parse_legacy(&entry.scalar("pkgmgr")) {
    return EntryBackend { kind, origin: PkgmgrOrigin::Recorded };
  }
  if let Some(kind) = PackageManagerKind::parse_legacy(&entry.scalar("source")) {
    return EntryBackend { kind, origin: PkgmgrOrigin::LegacySource };
  }
  if let Some(kind) = PackageManagerKind::parse_legacy(&entry.pkgfile) {
    return EntryBackend { kind, origin: PkgmgrOrigin::LegacyPkgfile };
  }
  if entry.pkgfile.ends_with(PKG_EXTENSION) {
    return EntryBackend { kind: PackageManagerKind::Pica, origin: PkgmgrOrigin::Default };
  }

  EntryBackend { kind: PackageManagerKind::Opkg, origin: PkgmgrOrigin::Default }
}

/// Resolves how an entry was obtained.
///
/// A row without a readable `source` dates from before the field carried a
/// channel value: an artifact path in `pkgfile` means Pica installed it from a
/// repository, anything else came from the backend's feeds.
#[must_use]
pub fn entry_source(entry: &DbEntry) -> EntrySource {
  if let Some(source) = EntrySource::parse(&entry.scalar("source")) {
    return source;
  }
  if entry.pkgfile.ends_with(PKG_EXTENSION) {
    return EntrySource::Repo;
  }

  EntrySource::Feed
}

#[cfg(test)]
mod tests {
  use super::{EntrySource, PkgmgrOrigin, entry_backend, entry_source};
  use crate::pkgmgr::PackageManagerKind;
  use pretty_assertions::assert_eq;
  use serde_json::json;

  fn entry(manifest: &serde_json::Value, pkgfile: &str) -> pica_pkg_core::model::db::DbEntry {
    serde_json::from_value(json!({ "manifest": manifest, "pkgfile": pkgfile })).expect("a record")
  }

  #[test]
  fn reads_a_recorded_backend() {
    let record = entry(&json!({ "pkgmgr": "apk", "source": "pica" }), "/tmp/demo.pkg.tar.gz");
    let resolved = entry_backend(&record);
    assert_eq!(resolved.kind, PackageManagerKind::Apk);
    assert_eq!(resolved.origin, PkgmgrOrigin::Recorded);
    assert!(!resolved.is_inferred());
    assert_eq!(entry_source(&record), EntrySource::Repo);
  }

  #[test]
  fn migrates_a_legacy_feed_install() {
    // `opkg`-era `-So` row: the backend was stored in `source`, no `pkgmgr`.
    let record = entry(&json!({ "source": "opkg" }), "opkg");
    let resolved = entry_backend(&record);
    assert_eq!(resolved.kind, PackageManagerKind::Opkg);
    assert_eq!(resolved.origin, PkgmgrOrigin::LegacySource);
    assert!(resolved.is_inferred());
    assert_eq!(entry_source(&record), EntrySource::Feed);
  }

  #[test]
  fn migrates_a_legacy_row_that_lost_source() {
    let record = entry(&json!({}), "apk");
    let resolved = entry_backend(&record);
    assert_eq!(resolved.kind, PackageManagerKind::Apk);
    assert_eq!(resolved.origin, PkgmgrOrigin::LegacyPkgfile);
    assert_eq!(entry_source(&record), EntrySource::Feed);
  }

  #[test]
  fn migrates_a_row_recorded_before_the_backend_was_renamed() {
    // `none` is what the `pica` backend was recorded as, so a row that still
    // says it has to read back as `pica`. Falling through instead would end at
    // the `opkg` default and hand it a payload it never installed.
    let record = entry(&json!({ "source": "feed", "pkgmgr": "none" }), "none");
    let resolved = entry_backend(&record);
    assert_eq!(resolved.kind, PackageManagerKind::Pica);
    assert_eq!(resolved.origin, PkgmgrOrigin::Recorded);
    assert_eq!(entry_source(&record), EntrySource::Feed);

    // The name may also be the only thing left in `pkgfile`.
    let record = entry(&json!({ "source": "feed" }), "none");
    let resolved = entry_backend(&record);
    assert_eq!(resolved.kind, PackageManagerKind::Pica);
    assert_eq!(resolved.origin, PkgmgrOrigin::LegacyPkgfile);
  }

  #[test]
  fn an_artifact_row_never_delegates_to_a_backend() {
    // The `opkg` default was injected at install time even for lifecycle
    // packages, so a row that names no backend must not run a package manager
    // against a payload Pica deployed itself.
    //
    // `pica` is both a channel name and a backend name, so a legacy `source`
    // that says `pica` is read as naming the backend; either reading ends at the
    // same place, which is what makes the upgrade path safe.
    let record = entry(&json!({ "source": "pica" }), "/tmp/demo.pkg.tar.gz");
    let resolved = entry_backend(&record);
    assert_eq!(resolved.kind, PackageManagerKind::Pica);
    assert_eq!(resolved.origin, PkgmgrOrigin::LegacySource);
    assert_eq!(entry_source(&record), EntrySource::Repo);

    // A channel that is not a backend name (`url`, `local`, `feed`) has to reach
    // the same answer through the artifact rule, because otherwise the `opkg`
    // fallback at the end of the ladder would claim it. Both that rule and the
    // fallback report `Default`, so the backend — not the origin — is what says
    // which one applied.
    for (channel, source) in
      [("url", EntrySource::Url), ("local", EntrySource::Local), ("feed", EntrySource::Feed)]
    {
      let record = entry(&json!({ "source": channel }), "/tmp/demo.pkg.tar.gz");
      let resolved = entry_backend(&record);
      assert_eq!(resolved.kind, PackageManagerKind::Pica, "source={channel}");
      assert_eq!(resolved.origin, PkgmgrOrigin::Default, "source={channel}");
      assert_eq!(entry_source(&record), source, "source={channel}");
    }
  }

  #[test]
  fn unreadable_fields_fall_through_instead_of_erroring() {
    // Repeatable fields parse as arrays; a malformed row must not be treated as
    // a valid backend name.
    let record = entry(&json!({ "pkgmgr": ["opkg", "apk"], "source": ["pica"] }), "opkg");
    let resolved = entry_backend(&record);
    assert_eq!(resolved.kind, PackageManagerKind::Opkg);
    assert_eq!(resolved.origin, PkgmgrOrigin::LegacyPkgfile);
    assert_eq!(entry_source(&record), EntrySource::Feed);
  }

  #[test]
  fn parses_every_channel_and_rejects_junk() {
    assert_eq!(EntrySource::parse("pica"), Some(EntrySource::Repo));
    assert_eq!(EntrySource::parse("local"), Some(EntrySource::Local));
    assert_eq!(EntrySource::parse("url"), Some(EntrySource::Url));
    assert_eq!(EntrySource::parse("feed"), Some(EntrySource::Feed));
    assert_eq!(EntrySource::parse(" APK "), Some(EntrySource::Feed));
    assert_eq!(EntrySource::parse(""), None);
    assert_eq!(EntrySource::parse("mirror"), None);
  }

  #[test]
  fn only_repository_installs_are_upgradable() {
    assert!(EntrySource::Repo.is_repo_resolvable());
    assert!(!EntrySource::Local.is_repo_resolvable());
    assert!(!EntrySource::Url.is_repo_resolvable());
    assert!(!EntrySource::Feed.is_repo_resolvable());
  }
}
