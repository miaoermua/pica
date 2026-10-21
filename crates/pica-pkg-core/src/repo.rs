use crate::error::{PicaError, PicaResult};
use crate::version::pkgver_cmp_key;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const REPO_SCHEMA: i64 = 2;
pub const PKG_EXTENSION: &str = ".pkg.tar.gz";
pub const SUPPORTED_PKGMGRS: [&str; 3] = ["opkg", "apk", "pica"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageIndex {
  pub schema: i64,
  pub packages: Vec<Package>,
}

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
  #[serde(default)]
  pub visibility: Option<String>,
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

/// # Errors
/// Returns an error if JSON parsing fails or validation detects invalid entries.
pub fn parse_repo_json(content: &str) -> PicaResult<PackageIndex> {
  let parsed: PackageIndex = serde_json::from_str(content)?;
  validate(&parsed)?;
  Ok(parsed)
}

/// # Errors
/// Returns an error if the schema version is wrong or any package entry is invalid.
pub fn validate(repo: &PackageIndex) -> PicaResult<()> {
  if repo.schema != REPO_SCHEMA {
    return Err(PicaError::msg(format!("schema must be {REPO_SCHEMA}")));
  }

  for pkg in &repo.packages {
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

    if !SUPPORTED_PKGMGRS.contains(&pkg.pkgmgr.as_str()) {
      return Err(PicaError::msg(format!(
        "package {}: unsupported pkgmgr {} (supported: {})",
        pkg.pkgname,
        pkg.pkgmgr,
        SUPPORTED_PKGMGRS.join(", ")
      )));
    }

    validate_filename(pkg)?;
    validate_sha256(pkg)?;

    if let Some(download_url) = &pkg.download_url {
      if !is_supported_url(download_url) {
        return Err(PicaError::msg(format!(
          "package {}: invalid download_url {}",
          pkg.pkgname, download_url
        )));
      }
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
  use super::{Package, REPO_SCHEMA, filename_suffix, package_filename, parse_repo_json};
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

  fn index(packages: &[Value]) -> String {
    json!({ "schema": REPO_SCHEMA, "packages": packages }).to_string()
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
  fn accepts_schema_two_with_the_pica_backend() {
    let parsed = parse_repo_json(&index(&[entry("hello-1.0.0-1-all-pica.pkg.tar.gz", "pica")]))
      .expect("valid pica repo");
    assert_eq!(parsed.packages[0].pkgmgr, "pica");
  }

  #[test]
  fn rejects_the_retired_none_backend() {
    let err = parse_repo_json(&index(&[entry("hello-1.0.0-1-all-none.pkg.tar.gz", "none")]))
      .expect_err("`none` was replaced by `pica`");
    assert!(err.to_string().contains("unsupported pkgmgr"), "{err}");
    assert!(err.to_string().contains("pica"), "the error must name the supported set: {err}");
  }

  #[test]
  fn accepts_schema_two_with_required_pkgmgr() {
    let parsed = parse_repo_json(&index(&[entry("hello-1.0.0-1-all-apk.pkg.tar.gz", "apk")]))
      .expect("valid apk repo");
    assert_eq!(parsed.packages.len(), 1);
    assert_eq!(parsed.packages[0].pkgmgr, "apk");
  }

  #[test]
  fn rejects_legacy_schema_one() {
    let text = json!({
      "schema": 1,
      "packages": [entry("hello-1.0.0-1-all-opkg.pkg.tar.gz", "opkg")],
    })
    .to_string();
    let err = parse_repo_json(&text).expect_err("schema 1 must be rejected");
    assert!(err.to_string().contains("schema must be 2"));
  }

  #[test]
  fn rejects_unknown_pkgmgr() {
    let err = parse_repo_json(&index(&[entry("hello-1.0.0-1-all-opkg.pkg.tar.gz", "pacman")]))
      .expect_err("must reject");
    assert!(err.to_string().contains("unsupported pkgmgr"));
  }

  #[test]
  fn rejects_apk_filename_that_disagrees_with_pkgmgr() {
    let err = parse_repo_json(&index(&[entry("hello-1.0.0-1-all-apk.pkg.tar.gz", "opkg")]))
      .expect_err("must reject");
    assert!(err.to_string().contains("filename mismatch"));
  }

  #[test]
  fn rejects_an_opkg_filename_from_before_the_suffix() {
    // The compatibility break this change makes: an index built before `-opkg`
    // must be regenerated, not silently accepted under a different on-disk name.
    let err = parse_repo_json(&index(&[entry("hello-1.0.0-1-all.pkg.tar.gz", "opkg")]))
      .expect_err("the old opkg filename must not validate");
    assert!(err.to_string().contains("filename mismatch"), "{err}");
    assert!(err.to_string().contains("-opkg.pkg.tar.gz"), "{err}");
  }

  #[test]
  fn reject_invalid_filename() {
    assert!(parse_repo_json(&index(&[entry("../hello.pkg.tar.gz", "opkg")])).is_err());
  }

  #[test]
  fn reject_invalid_sha256() {
    let mut pkg = entry("hello-1.0.0-1-all-opkg.pkg.tar.gz", "opkg");
    pkg["sha256"] = json!("xyz");
    assert!(parse_repo_json(&index(&[pkg])).is_err());
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
    let parsed = parse_repo_json(&index(&[pkg])).expect("valid repo");
    let reserialized = serde_json::to_value(&parsed).expect("serialize");
    assert_eq!(
      reserialized["packages"][0]["future_field"],
      json!({"nested": [1, 2, 3]}),
      "sync must not strip fields the struct does not model"
    );
  }
}
