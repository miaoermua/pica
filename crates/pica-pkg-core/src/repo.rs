use crate::error::{PicaError, PicaResult};
pub use crate::layout::{PKG_EXTENSION, SUPPORTED_PKGMGRS};
use crate::manifest::{SOURCE_AVAILABILITY_VALUES, is_valid_source_availability};
use crate::naming::{is_valid_appname, is_valid_pkgname};
use crate::version::pkgver_cmp_key;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Package {
  pub pkgname: String,
  pub pkgver: String,
  pub pkgrel: String,
  /// Backend this artifact was built for. Required: it is the only authority on
  /// backend identity, never re-derived from `filename` or `manifest`.
  pub pkgmgr: String,
  pub platform: String,
  pub arch: String,
  pub filename: String,
  pub sha256: String,
  #[serde(default)]
  pub appname: Option<String>,
  #[serde(default)]
  pub os: Option<String>,
  #[serde(default)]
  pub branch: Option<String>,
  #[serde(default)]
  pub protocol: Option<String>,
  #[serde(default)]
  pub url: Option<String>,
  #[serde(default, alias = "origin")]
  pub origin: Option<String>,
  #[serde(default)]
  pub luci_url: Option<String>,
  #[serde(default)]
  pub luci_desc: Option<String>,
  #[serde(default)]
  pub pkgdesc: Option<String>,
  #[serde(default)]
  pub license: Option<String>,
  /// How much of the package's source is publicly available. Optional: an
  /// absent or empty value means the entry does not declare it.
  #[serde(default)]
  pub source_availability: Option<String>,
  #[serde(default)]
  pub pica: Option<String>,
  #[serde(default)]
  pub download_url: Option<String>,
  /// Unpacked size of the package's own `root/` payload, in bytes.
  #[serde(default)]
  pub size: Option<u64>,
  /// Compressed archive size, in bytes. Lives in the index, not the manifest,
  /// because it cannot be known before the archive exists.
  #[serde(default)]
  pub csize: Option<u64>,
  #[serde(default)]
  pub manifest: Option<Value>,
  /// Keeps unknown fields alive across a parse/serialize round-trip. Without
  /// this, `pica -S` silently strips any field the struct does not know about.
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

impl Package {
  #[must_use]
  pub fn app_key(&self) -> &str {
    self.appname.as_deref().filter(|value| !value.is_empty()).unwrap_or(&self.pkgname)
  }

  #[must_use]
  pub fn os_key(&self) -> &str {
    self.os.as_deref().filter(|value| !value.is_empty()).unwrap_or("all")
  }

  #[must_use]
  pub fn version_key(&self) -> String {
    pkgver_cmp_key(&self.pkgver, &self.pkgrel)
  }
}

/// Version of the entry descriptor (`index.json`) at a source root.
pub const ENTRY_SCHEMA: i64 = 1;
/// Version of one backend's index, reachable from an entry descriptor.
pub const BACKEND_INDEX_SCHEMA: i64 = 1;

/// Where one backend's index lives, relative to the entry descriptor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryManager {
  pub path: String,
}

/// The `index.json` a source root serves: which backends it provides and where
/// their indexes are.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryDescriptor {
  pub schema: i64,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub name: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub description: Option<String>,
  pub managers: std::collections::BTreeMap<String, EntryManager>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

/// One backend's index, as named by an entry descriptor. It is the only place a
/// source publishes packages; there is no other file format to fall back to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackendIndex {
  pub schema: i64,
  pub mgr: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub generated_at: Option<String>,
  pub packages: Vec<Package>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

/// # Errors
/// Returns an error when the descriptor is not schema 1, declares no manager,
/// names an unsupported backend, or gives a path that is not safely relative.
pub fn parse_entry_descriptor(content: &str) -> PicaResult<EntryDescriptor> {
  let parsed: EntryDescriptor = serde_json::from_str(content)?;
  if parsed.schema != ENTRY_SCHEMA {
    return Err(PicaError::msg(format!("entry descriptor schema must be {ENTRY_SCHEMA}")));
  }
  if parsed.managers.is_empty() {
    return Err(PicaError::msg("entry descriptor declares no managers"));
  }

  for (mgr, entry) in &parsed.managers {
    if !SUPPORTED_PKGMGRS.contains(&mgr.as_str()) {
      return Err(PicaError::msg(format!(
        "entry descriptor: unsupported manager {mgr} (supported: {})",
        SUPPORTED_PKGMGRS.join(", ")
      )));
    }
    if let Some(reason) = relative_path_problem(&entry.path) {
      return Err(PicaError::msg(format!(
        "entry descriptor: manager {mgr} path {:?}: {reason}",
        entry.path
      )));
    }
  }

  Ok(parsed)
}

/// # Errors
/// Returns an error when the index is not schema 1, belongs to another backend
/// than `expected_mgr`, or carries a package that belongs to another backend.
/// The `mgr` check is the second half of package identity: a source cannot
/// smuggle an `opkg` entry into its `apk` index, and a client never has to
/// re-derive a backend from a filename.
pub fn parse_backend_index(content: &str, expected_mgr: &str) -> PicaResult<BackendIndex> {
  let parsed = parse_backend_envelope(content, expected_mgr)?;
  check_packages(&parsed)?;
  Ok(parsed)
}

/// An index entry left out because of its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedPackage {
  /// The manifest field at fault: `pkgname` or `appname`.
  pub field: &'static str,
  /// The name as the index wrote it. It is untrusted text: quote it when
  /// showing it.
  pub value: String,
}

/// [`parse_backend_index`] for a reader that can use the rest of an index.
///
/// One entry with an unusable name says nothing about its neighbours, so it is
/// left out and reported. Every other rule still fails the whole index.
///
/// # Errors
/// Returns an error for the same reasons as [`parse_backend_index`], apart from
/// a `pkgname` or `appname` that is present but invalid.
pub fn parse_backend_index_skipping(
  content: &str,
  expected_mgr: &str,
) -> PicaResult<(BackendIndex, Vec<SkippedPackage>)> {
  let mut parsed = parse_backend_envelope(content, expected_mgr)?;

  let mut skipped = Vec::new();
  parsed.packages.retain(|package| match name_problem(package) {
    Some(problem) => {
      skipped.push(problem);
      false
    }
    None => true,
  });

  check_packages(&parsed)?;
  Ok((parsed, skipped))
}

/// The invalid name of an entry, if it has one. An empty `pkgname` is not a
/// name problem: it is a missing required field.
fn name_problem(package: &Package) -> Option<SkippedPackage> {
  if !package.pkgname.is_empty() && !is_valid_pkgname(&package.pkgname) {
    return Some(SkippedPackage { field: "pkgname", value: package.pkgname.clone() });
  }
  let appname = package.appname.as_deref().filter(|value| !value.is_empty())?;
  (!is_valid_appname(appname))
    .then(|| SkippedPackage { field: "appname", value: appname.to_string() })
}

fn parse_backend_envelope(content: &str, expected_mgr: &str) -> PicaResult<BackendIndex> {
  let mut parsed: BackendIndex = serde_json::from_str(content)?;
  if parsed.schema != BACKEND_INDEX_SCHEMA {
    return Err(PicaError::msg(format!(
      "backend index schema must be {BACKEND_INDEX_SCHEMA}, got {}",
      parsed.schema
    )));
  }
  if parsed.mgr != expected_mgr {
    return Err(PicaError::msg(format!(
      "backend index declares manager {}, but it was fetched as {expected_mgr}",
      parsed.mgr
    )));
  }

  // `visibility` is the retired spelling of `source_availability`. It is not
  // unknown data to forward: an index re-serialized from this parse (the
  // client's cache, a packer's output) must not keep the old name alive. Drop
  // it from the entry and the manifest it embeds; the entry then simply
  // declares no availability.
  for package in &mut parsed.packages {
    package.extra.remove("visibility");
    if let Some(manifest) = package.manifest.as_mut().and_then(|value| value.as_object_mut()) {
      manifest.remove("visibility");
    }
  }

  Ok(parsed)
}

fn check_packages(index: &BackendIndex) -> PicaResult<()> {
  for pkg in &index.packages {
    if pkg.pkgmgr != index.mgr {
      return Err(PicaError::msg(format!(
        "package {}: pkgmgr {} does not match the index's {}",
        pkg.pkgname, pkg.pkgmgr, index.mgr
      )));
    }
  }

  validate_packages(&index.packages)
}

/// Whether `path` is a relative path a source may hand a client: no scheme, no
/// absolute root, no parent segment, no encoding tricks, no query or fragment.
///
/// Returns the reason it is unusable, so the caller can say why instead of
/// printing a bare rejection.
#[must_use]
pub fn relative_path_problem(path: &str) -> Option<&'static str> {
  path_problem(path, |_| false)
}

/// [`relative_path_problem`] for the path of an artifact.
///
/// An artifact's filename carries the `pkgname` and the version, so `@`, `+` and
/// `~` have to be allowed. All three are legal in a URL path as they stand.
#[must_use]
pub fn artifact_path_problem(path: &str) -> Option<&'static str> {
  path_problem(path, |ch| matches!(ch, '+' | '@' | '~'))
}

fn path_problem(path: &str, also: impl Fn(char) -> bool) -> Option<&'static str> {
  if path.is_empty() {
    return Some("empty path");
  }
  if path.starts_with('/') || path.starts_with('\\') {
    return Some("must not start at the root");
  }
  if path.contains('\\') {
    return Some("must use / as its separator");
  }
  if path.contains('%') {
    return Some("must not use percent-encoding");
  }
  if path.contains('?') || path.contains('#') {
    return Some("must not carry a query or fragment");
  }
  if path.contains(':') {
    return Some("must not carry a scheme or a drive letter");
  }

  for segment in path.split('/') {
    if segment.is_empty() {
      return Some("must not contain an empty segment");
    }
    if segment == "." || segment == ".." {
      return Some("must not contain . or ..");
    }
    if !segment
      .chars()
      .all(|ch| ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-' || also(ch))
    {
      return Some("may only use ASCII letters, digits and the punctuation allowed here");
    }
  }

  None
}

fn validate_download_url(pkgname: &str, value: &str) -> PicaResult<()> {
  if is_supported_url(value) {
    return Ok(());
  }

  if let Some(reason) = artifact_path_problem(value) {
    return Err(PicaError::msg(format!(
      "package {pkgname}: invalid download_url {value:?}: {reason}"
    )));
  }

  Ok(())
}

/// # Errors
/// Returns an error naming the first package entry that breaks the rules every
/// index shares, whichever file it came from.
pub fn validate_packages(packages: &[Package]) -> PicaResult<()> {
  for pkg in packages {
    if pkg.pkgname.is_empty()
      || pkg.pkgver.is_empty()
      || pkg.pkgrel.is_empty()
      || pkg.platform.is_empty()
      || pkg.arch.is_empty()
      || pkg.filename.is_empty()
      || pkg.sha256.is_empty()
    {
      return Err(PicaError::msg(
        "package entry missing required string fields: pkgname/pkgver/pkgrel/platform/arch/filename/sha256",
      ));
    }

    if !is_valid_pkgname(&pkg.pkgname) {
      return Err(PicaError::msg(format!("package {:?}: invalid pkgname", pkg.pkgname)));
    }
    if let Some(appname) = pkg.appname.as_deref().filter(|value| !value.is_empty()) {
      if !is_valid_appname(appname) {
        return Err(PicaError::msg(format!(
          "package {}: invalid appname {appname:?}",
          pkg.pkgname
        )));
      }
    }

    if !SUPPORTED_PKGMGRS.contains(&pkg.pkgmgr.as_str()) {
      return Err(PicaError::msg(format!(
        "package {}: unsupported pkgmgr {} (supported: {})",
        pkg.pkgname,
        pkg.pkgmgr,
        SUPPORTED_PKGMGRS.join(", ")
      )));
    }

    if let Some(value) = &pkg.source_availability {
      if !is_valid_source_availability(value) {
        return Err(PicaError::msg(format!(
          "package {}: invalid source_availability value: {} (supported: {})",
          pkg.pkgname,
          value,
          SOURCE_AVAILABILITY_VALUES.join(", ")
        )));
      }
    }

    validate_filename(pkg)?;
    validate_sha256(pkg)?;

    if let Some(download_url) = &pkg.download_url {
      validate_download_url(pkg.pkgname.as_str(), download_url)?;
    }
  }

  Ok(())
}

fn validate_sha256(pkg: &Package) -> PicaResult<()> {
  let value = pkg.sha256.trim();
  if value.len() != 64 || !value.chars().all(|ch| ch.is_ascii_hexdigit()) {
    return Err(PicaError::msg(format!("package {}: invalid sha256 {}", pkg.pkgname, pkg.sha256)));
  }

  Ok(())
}

fn validate_filename(pkg: &Package) -> PicaResult<()> {
  let filename = &pkg.filename;
  if filename.contains('/') || filename.contains("..") || !filename.ends_with(PKG_EXTENSION) {
    return Err(PicaError::msg(format!("package {}: invalid filename {}", pkg.pkgname, filename)));
  }

  let expected = package_filename(
    &pkg.pkgname,
    &pkg.pkgver,
    &pkg.pkgrel,
    &pkg.platform,
    &pkg.arch,
    &pkg.pkgmgr,
  )?;
  if expected != *filename {
    return Err(PicaError::msg(format!(
      "package {}: filename mismatch, expected {}, got {}",
      pkg.pkgname, expected, filename
    )));
  }

  Ok(())
}

/// Backend marker carried by the filename. Purely cosmetic: `pkgmgr` in the
/// index is the authority, and this suffix is never parsed back out.
///
/// Every supported backend has an explicit suffix. `None` is an error: naming
/// an unknown backend the way `opkg` used to be named (no suffix) would let two
/// backends write the same filename.
#[must_use = "an unknown backend is a fact the caller must handle"]
pub fn filename_suffix(pkgmgr: &str) -> Option<&'static str> {
  match pkgmgr {
    "opkg" => Some("-opkg"),
    "apk" => Some("-apk"),
    "pica" => Some("-pica"),
    _ => None,
  }
}

/// # Errors
/// Returns an error when `pkgmgr` is not one of [`SUPPORTED_PKGMGRS`], so an
/// unknown backend can never be given an `opkg`-shaped name.
pub fn package_filename(
  pkgname: &str,
  pkgver: &str,
  pkgrel: &str,
  platform: &str,
  arch: &str,
  pkgmgr: &str,
) -> PicaResult<String> {
  let suffix = filename_suffix(pkgmgr).ok_or_else(|| {
    PicaError::msg(format!(
      "unsupported pkgmgr {pkgmgr:?} for a package filename (supported: {})",
      SUPPORTED_PKGMGRS.join(", ")
    ))
  })?;
  let stem = if platform == "all" {
    format!("{pkgname}-{pkgver}-{pkgrel}-{arch}")
  } else {
    format!("{pkgname}-{pkgver}-{pkgrel}-{platform}-{arch}")
  };
  Ok(format!("{stem}{suffix}{PKG_EXTENSION}"))
}

#[must_use]
pub fn is_supported_url(value: &str) -> bool {
  value.starts_with("http://") || value.starts_with("https://") || value.starts_with("file://")
}

#[cfg(test)]
mod tests {
  use super::{
    BACKEND_INDEX_SCHEMA, Package, SkippedPackage, artifact_path_problem, filename_suffix,
    package_filename, parse_backend_index, parse_backend_index_skipping, parse_entry_descriptor,
    relative_path_problem,
  };
  use serde_json::{Value, json};

  fn entry(filename: &str, pkgmgr: &str) -> Value {
    json!({
      "pkgname": "hello",
      "pkgver": "1.0.0",
      "pkgrel": "1",
      "pkgmgr": pkgmgr,
      "platform": "all",
      "arch": "all",
      "filename": filename,
      "sha256": "1".repeat(64),
    })
  }

  #[test]
  fn an_artifact_path_may_carry_what_a_filename_carries() {
    let path = "pica/ddns-go@immortalwrt-1.0+b~1-1-all-pica.pkg.tar.gz";
    assert_eq!(artifact_path_problem(path), None);
    assert!(relative_path_problem(path).is_some(), "a manager path stays as strict as it was");
  }

  #[test]
  fn an_artifact_path_still_cannot_leave_the_source() {
    for path in ["../demo.pkg.tar.gz", "/demo.pkg.tar.gz", "a/../b", "a%2fb", "a?b", "a:b"] {
      assert!(artifact_path_problem(path).is_some(), "{path}");
    }
  }

  #[test]
  fn an_index_entry_with_an_unsafe_name_is_refused() {
    let mut bad_pkgname = entry("x", "pica");
    bad_pkgname["pkgname"] = json!("../evil");
    let mut bad_appname = entry("hello-1.0.0-1-all-pica.pkg.tar.gz", "pica");
    bad_appname["appname"] = json!("hello@alice");

    for package in [bad_pkgname, bad_appname] {
      let index = json!({ "schema": 1, "mgr": "pica", "packages": [package] }).to_string();
      let err = parse_backend_index(&index, "pica").expect_err("the name must be refused");
      assert!(err.to_string().contains("invalid"), "{err}");
    }
  }

  #[test]
  fn an_entry_with_an_unsafe_name_costs_only_that_entry() {
    let good = entry("hello-1.0.0-1-all-pica.pkg.tar.gz", "pica");
    let mut bad_pkgname = entry("x", "pica");
    bad_pkgname["pkgname"] = json!("../evil");
    let mut bad_appname = entry("hello-1.0.0-1-all-pica.pkg.tar.gz", "pica");
    bad_appname["appname"] = json!("hello@alice");
    let index = json!({ "schema": 1, "mgr": "pica", "packages": [bad_pkgname, good, bad_appname] })
      .to_string();

    let (parsed, skipped) =
      parse_backend_index_skipping(&index, "pica").expect("the rest is valid");

    assert_eq!(parsed.packages.len(), 1);
    assert_eq!(parsed.packages[0].pkgname, "hello");
    assert_eq!(
      skipped,
      [
        SkippedPackage { field: "pkgname", value: "../evil".to_string() },
        SkippedPackage { field: "appname", value: "hello@alice".to_string() },
      ]
    );
  }

  #[test]
  fn every_other_broken_rule_still_fails_the_whole_index() {
    let mut missing_pkgname = entry("x.pkg.tar.gz", "pica");
    missing_pkgname["pkgname"] = json!("");
    let mut bad_sha = entry("hello-1.0.0-1-all-pica.pkg.tar.gz", "pica");
    bad_sha["sha256"] = json!("nope");
    let wrong_filename = entry("other-1.0.0-1-all-pica.pkg.tar.gz", "pica");
    let other_backend = entry("hello-1.0.0-1-all-apk.pkg.tar.gz", "apk");

    for package in [missing_pkgname, bad_sha, wrong_filename, other_backend] {
      let index = json!({ "schema": 1, "mgr": "pica", "packages": [package] }).to_string();
      assert!(parse_backend_index_skipping(&index, "pica").is_err(), "{index}");
    }
  }

  #[test]
  fn every_backend_has_an_explicit_suffix() {
    assert_eq!(filename_suffix("opkg"), Some("-opkg"));
    assert_eq!(filename_suffix("apk"), Some("-apk"));
    assert_eq!(filename_suffix("pica"), Some("-pica"));
    assert_eq!(filename_suffix("pacman"), None, "an unknown backend has no opkg-shaped name");
  }

  #[test]
  fn filename_carries_backend_suffix() {
    assert_eq!(
      package_filename("hello", "1.0.0", "1", "all", "all", "opkg").expect("opkg filename"),
      "hello-1.0.0-1-all-opkg.pkg.tar.gz"
    );
    assert_eq!(
      package_filename("hello", "1.0.0", "1", "all", "all", "apk").expect("apk filename"),
      "hello-1.0.0-1-all-apk.pkg.tar.gz"
    );
    assert_eq!(
      package_filename("hello", "1.0.0", "1", "all", "all", "pica").expect("pica filename"),
      "hello-1.0.0-1-all-pica.pkg.tar.gz"
    );
    assert_eq!(
      package_filename("demo", "1.0", "1", "amd64", "x86_64", "opkg").expect("opkg filename"),
      "demo-1.0-1-amd64-x86_64-opkg.pkg.tar.gz"
    );
    assert_eq!(
      package_filename("hello", "1.0.0", "1", "arm64", "aarch64_cortex-a53", "apk")
        .expect("apk filename"),
      "hello-1.0.0-1-arm64-aarch64_cortex-a53-apk.pkg.tar.gz"
    );
    assert_eq!(
      package_filename("hello", "1.0.0", "1", "arm64", "aarch64_cortex-a53", "pica")
        .expect("pica filename"),
      "hello-1.0.0-1-arm64-aarch64_cortex-a53-pica.pkg.tar.gz"
    );
  }

  #[test]
  fn an_unknown_backend_cannot_be_named() {
    let err = package_filename("hello", "1.0.0", "1", "all", "all", "pacman")
      .expect_err("an unknown backend must not get a filename");
    assert!(err.to_string().contains("unsupported pkgmgr"), "{err}");
    assert!(err.to_string().contains("opkg, apk, pica"), "{err}");
  }

  #[test]
  fn pica_and_opkg_artifacts_do_not_collide() {
    let opkg = package_filename("demo", "1.0.0", "1", "all", "all", "opkg").expect("opkg");
    let apk = package_filename("demo", "1.0.0", "1", "all", "all", "apk").expect("apk");
    let pica = package_filename("demo", "1.0.0", "1", "all", "all", "pica").expect("pica");
    assert_ne!(opkg, pica);
    assert_ne!(opkg, apk);
    assert_ne!(apk, pica);
  }

  #[test]
  fn accepts_a_pica_backend_index() {
    let parsed = parse_backend_index(
      &backend_index("pica", &[entry("hello-1.0.0-1-all-pica.pkg.tar.gz", "pica")]),
      "pica",
    )
    .expect("valid pica index");
    assert_eq!(parsed.packages[0].pkgmgr, "pica");
  }

  #[test]
  fn rejects_the_retired_none_backend() {
    let err = parse_backend_index(
      &backend_index("none", &[entry("hello-1.0.0-1-all-none.pkg.tar.gz", "none")]),
      "none",
    )
    .expect_err("`none` was replaced by `pica`");
    assert!(err.to_string().contains("unsupported pkgmgr"), "{err}");
    assert!(err.to_string().contains("pica"), "the error must name the supported set: {err}");
  }

  #[test]
  fn accepts_an_apk_backend_index() {
    let parsed = parse_backend_index(
      &backend_index("apk", &[entry("hello-1.0.0-1-all-apk.pkg.tar.gz", "apk")]),
      "apk",
    )
    .expect("valid apk index");
    assert_eq!(parsed.packages.len(), 1);
    assert_eq!(parsed.packages[0].pkgmgr, "apk");
  }

  #[test]
  fn rejects_the_retired_schema_two_index() {
    let text = json!({
      "schema": 2,
      "mgr": "opkg",
      "packages": [entry("hello-1.0.0-1-all-opkg.pkg.tar.gz", "opkg")],
    })
    .to_string();
    let err = parse_backend_index(&text, "opkg").expect_err("schema 2 must be rejected");
    assert!(err.to_string().contains("schema must be 1"), "{err}");
  }

  #[test]
  fn rejects_unknown_pkgmgr() {
    let err = parse_backend_index(
      &backend_index("pacman", &[entry("hello-1.0.0-1-all-opkg.pkg.tar.gz", "pacman")]),
      "pacman",
    )
    .expect_err("must reject");
    assert!(err.to_string().contains("unsupported pkgmgr"));
  }

  #[test]
  fn rejects_apk_filename_that_disagrees_with_pkgmgr() {
    let err = parse_backend_index(
      &backend_index("opkg", &[entry("hello-1.0.0-1-all-apk.pkg.tar.gz", "opkg")]),
      "opkg",
    )
    .expect_err("must reject");
    assert!(err.to_string().contains("filename mismatch"));
  }

  #[test]
  fn rejects_an_opkg_filename_from_before_the_suffix() {
    // The compatibility break this change makes: an index built before `-opkg`
    // must be regenerated, not silently accepted under a different on-disk name.
    let err = parse_backend_index(
      &backend_index("opkg", &[entry("hello-1.0.0-1-all.pkg.tar.gz", "opkg")]),
      "opkg",
    )
    .expect_err("the old opkg filename must not validate");
    assert!(err.to_string().contains("filename mismatch"), "{err}");
    assert!(err.to_string().contains("-opkg.pkg.tar.gz"), "{err}");
  }

  #[test]
  fn reject_invalid_filename() {
    assert!(
      parse_backend_index(&backend_index("opkg", &[entry("../hello.pkg.tar.gz", "opkg")]), "opkg")
        .is_err()
    );
  }

  #[test]
  fn reject_invalid_sha256() {
    let mut pkg = entry("hello-1.0.0-1-all-opkg.pkg.tar.gz", "opkg");
    pkg["sha256"] = json!("xyz");
    assert!(parse_backend_index(&backend_index("opkg", &[pkg]), "opkg").is_err());
  }

  #[test]
  fn app_key_falls_back_to_pkgname() {
    let pkg: Package = serde_json::from_value(entry("hello-1.0.0-1-all-opkg.pkg.tar.gz", "opkg"))
      .expect("deserialize");
    assert_eq!(pkg.app_key(), "hello");
    assert_eq!(pkg.os_key(), "all");
  }

  #[test]
  fn unknown_fields_survive_round_trip() {
    let mut pkg = entry("hello-1.0.0-1-all-opkg.pkg.tar.gz", "opkg");
    pkg["future_field"] = json!({"nested": [1, 2, 3]});
    let parsed = parse_backend_index(&backend_index("opkg", &[pkg]), "opkg").expect("valid index");
    let reserialized = serde_json::to_value(&parsed).expect("serialize");
    assert_eq!(
      reserialized["packages"][0]["future_field"],
      json!({"nested": [1, 2, 3]}),
      "sync must not strip fields the struct does not model"
    );
  }

  #[test]
  fn source_availability_round_trips_under_its_own_key() {
    let mut pkg = entry("hello-1.0.0-1-all-opkg.pkg.tar.gz", "opkg");
    pkg["source_availability"] = json!("partial");
    let parsed = parse_backend_index(&backend_index("opkg", &[pkg]), "opkg").expect("valid index");
    assert_eq!(parsed.packages[0].source_availability.as_deref(), Some("partial"));

    let reserialized = serde_json::to_value(&parsed).expect("serialize");
    assert_eq!(reserialized["packages"][0]["source_availability"], json!("partial"));
    assert!(
      reserialized["packages"][0].get("visibility").is_none(),
      "the retired key must not be written back"
    );
  }

  #[test]
  fn every_declared_source_availability_is_accepted() {
    for value in ["open", "partial", "closed"] {
      let mut pkg = entry("hello-1.0.0-1-all-opkg.pkg.tar.gz", "opkg");
      pkg["source_availability"] = json!(value);
      let parsed =
        parse_backend_index(&backend_index("opkg", &[pkg]), "opkg").expect("declared value");
      assert_eq!(parsed.packages[0].source_availability.as_deref(), Some(value));
    }
  }

  #[test]
  fn an_index_refuses_an_unknown_source_availability() {
    for value in ["mix", "proprietary", "OPEN"] {
      let mut pkg = entry("hello-1.0.0-1-all-opkg.pkg.tar.gz", "opkg");
      pkg["source_availability"] = json!(value);
      let err = parse_backend_index(&backend_index("opkg", &[pkg]), "opkg")
        .expect_err("an undeclared value must not reach an install");
      assert!(err.to_string().contains("source_availability"), "{value}: {err}");
      assert!(err.to_string().contains("open, partial, closed"), "{value}: {err}");
    }
  }

  #[test]
  fn an_index_may_omit_or_leave_source_availability_empty() {
    let pkg = entry("hello-1.0.0-1-all-opkg.pkg.tar.gz", "opkg");
    let parsed =
      parse_backend_index(&backend_index("opkg", &[pkg]), "opkg").expect("omitted is valid");
    assert_eq!(parsed.packages[0].source_availability, None);

    let mut pkg = entry("hello-1.0.0-1-all-opkg.pkg.tar.gz", "opkg");
    pkg["source_availability"] = json!("");
    let parsed =
      parse_backend_index(&backend_index("opkg", &[pkg]), "opkg").expect("empty is unset");
    assert_eq!(parsed.packages[0].source_availability.as_deref(), Some(""));
  }

  #[test]
  fn the_retired_field_does_not_survive_an_index_round_trip() {
    let mut pkg = entry("hello-1.0.0-1-all-opkg.pkg.tar.gz", "opkg");
    pkg["visibility"] = json!("mix");
    pkg["manifest"] = json!({"visibility": "mix"});
    let parsed =
      parse_backend_index(&backend_index("opkg", &[pkg]), "opkg").expect("an old index is read");
    assert_eq!(
      parsed.packages[0].source_availability, None,
      "an old value must not be reinterpreted as the new field"
    );

    let reserialized = serde_json::to_value(&parsed).expect("serialize");
    assert!(
      reserialized["packages"][0].get("visibility").is_none(),
      "the retired key must not be written back"
    );
    assert!(
      reserialized["packages"][0]["manifest"].get("visibility").is_none(),
      "the retired key must not survive inside the embedded manifest"
    );
  }

  fn descriptor(managers: &Value) -> String {
    json!({
      "schema": super::ENTRY_SCHEMA,
      "name": "official",
      "description": "test source",
      "managers": managers,
    })
    .to_string()
  }

  #[test]
  fn an_entry_descriptor_says_which_backends_a_source_provides() {
    let parsed = parse_entry_descriptor(&descriptor(&json!({
      "opkg": {"path": "opkg/index.json"},
      "pica": {"path": "pica/index.json"},
    })))
    .expect("valid descriptor");

    assert_eq!(
      parsed.managers.keys().collect::<Vec<_>>(),
      vec!["opkg", "pica"],
      "the descriptor is the only place a source advertises its backends"
    );
    assert_eq!(parsed.managers["opkg"].path, "opkg/index.json");
  }

  #[test]
  fn an_entry_descriptor_may_not_name_a_backend_we_cannot_install_with() {
    let err =
      parse_entry_descriptor(&descriptor(&json!({"pacman": {"path": "pacman/index.json"}})))
        .expect_err("undeclared backend");
    assert!(err.to_string().contains("unsupported manager pacman"), "{err}");
  }

  #[test]
  fn an_entry_descriptor_may_not_point_outside_itself() {
    for path in [
      "../secret/index.json",
      "/etc/index.json",
      "https://elsewhere.invalid/index.json",
      "opkg/../../index.json",
      "opkg%2f..%2findex.json",
      "opkg/index.json?x=1",
      "opkg\\index.json",
      "",
    ] {
      let err =
        parse_entry_descriptor(&descriptor(&json!({"opkg": {"path": path}}))).expect_err(path);
      assert!(
        err.to_string().contains("path"),
        "the error must say which field is wrong for {path:?}: {err}"
      );
    }
  }

  #[test]
  fn an_entry_descriptor_must_declare_something() {
    let err = parse_entry_descriptor(&descriptor(&json!({}))).expect_err("empty managers");
    assert!(err.to_string().contains("declares no managers"), "{err}");
  }

  fn backend_index(mgr: &str, packages: &[Value]) -> String {
    json!({
      "schema": BACKEND_INDEX_SCHEMA,
      "mgr": mgr,
      "generated_at": "2026-10-20T00:00:00Z",
      "packages": packages,
    })
    .to_string()
  }

  #[test]
  fn a_backend_index_is_read_as_the_backend_that_was_asked_for() {
    let parsed = parse_backend_index(
      &backend_index("opkg", &[entry("hello-1.0.0-1-all-opkg.pkg.tar.gz", "opkg")]),
      "opkg",
    )
    .expect("valid opkg index");

    assert_eq!(parsed.mgr, "opkg");
    assert_eq!(parsed.packages.len(), 1);
  }

  #[test]
  fn a_backend_index_for_another_backend_is_not_used() {
    let err = parse_backend_index(
      &backend_index("opkg", &[entry("hello-1.0.0-1-all-opkg.pkg.tar.gz", "opkg")]),
      "apk",
    )
    .expect_err("mgr must match the path it came from");
    assert!(err.to_string().contains("declares manager opkg"), "{err}");
  }

  #[test]
  fn a_backend_index_may_not_smuggle_in_another_backends_package() {
    let err = parse_backend_index(
      &backend_index("apk", &[entry("hello-1.0.0-1-all-opkg.pkg.tar.gz", "opkg")]),
      "apk",
    )
    .expect_err("same name in another backend is another package");
    assert!(err.to_string().contains("does not match"), "{err}");
  }

  #[test]
  fn a_backend_index_is_held_to_the_package_rules() {
    let mut pkg = entry("hello-1.0.0-1-all-opkg.pkg.tar.gz", "opkg");
    pkg["sha256"] = json!("xyz");
    let err = parse_backend_index(&backend_index("opkg", &[pkg]), "opkg").expect_err("bad hash");
    assert!(err.to_string().contains("sha256"), "{err}");
  }

  #[test]
  fn a_download_url_may_be_relative_to_the_source_root() {
    let mut pkg = entry("hello-1.0.0-1-all-opkg.pkg.tar.gz", "opkg");
    pkg["download_url"] = json!("opkg/hello-1.0.0-1-all-opkg.pkg.tar.gz");
    parse_backend_index(&backend_index("opkg", &[pkg]), "opkg")
      .expect("a relative download_url is resolved by the client");
  }

  #[test]
  fn a_download_url_may_not_climb_out_of_the_source() {
    for bad in ["../../etc/passwd", "/etc/passwd", "opkg/../../x.pkg.tar.gz", "opkg%2f..%2fx"] {
      let mut pkg = entry("hello-1.0.0-1-all-opkg.pkg.tar.gz", "opkg");
      pkg["download_url"] = json!(bad);
      let err = parse_backend_index(&backend_index("opkg", &[pkg]), "opkg").expect_err(bad);
      assert!(err.to_string().contains("download_url"), "{bad}: {err}");
    }
  }

  #[test]
  fn path_problems_are_named_for_the_user() {
    assert_eq!(relative_path_problem("apk/index.json"), None);
    assert_eq!(relative_path_problem("index.json"), None);
    assert!(relative_path_problem("../index.json").is_some());
    assert!(relative_path_problem("a//b").is_some());
    assert!(relative_path_problem("/index.json").is_some());
  }
}
