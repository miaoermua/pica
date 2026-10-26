use pica_pkg_core::error::PicaResult;
use pica_pkg_core::io::now_unix_secs;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

/// Fields the packer owns: they are recomputed per artifact, so any author
/// value is dropped rather than trusted. `targets` is a build-time-only field
/// and must not leak into an installed manifest.
const OWNED_KEYS: [&str; 9] =
  ["builddate", "size", "csize", "platform", "arch", "pkgver", "pkgrel", "pkgmgr", "targets"];

/// Fields the packer recomputes for one build target. Grouping them keeps the
/// per-target values together instead of as a long argument list that is easy to
/// transpose at the call site.
pub(crate) struct BuiltFields<'a> {
  pub pkgver: &'a str,
  pub pkgrel: &'a str,
  pub platform: &'a str,
  pub arch: &'a str,
  pub pkgmgr: &'a str,
  pub size: u64,
}

pub(crate) fn rewrite_manifest_for_build(
  source: &Path,
  target: &Path,
  fields: &BuiltFields<'_>,
) -> PicaResult<()> {
  let content = fs::read_to_string(source)?;
  let mut lines = Vec::new();
  let remove_keys: HashSet<&str> = OWNED_KEYS.into_iter().collect();

  for raw_line in content.lines() {
    let trimmed = raw_line.trim_start();
    if trimmed.starts_with('#') {
      lines.push(raw_line.to_string());
      continue;
    }

    let Some((raw_key, _)) = raw_line.split_once('=') else {
      lines.push(raw_line.to_string());
      continue;
    };

    if remove_keys.contains(raw_key.trim()) {
      continue;
    }
    lines.push(raw_line.to_string());
  }

  lines.push(format!("builddate = {}", now_unix_secs()));
  lines.push(format!("pkgver = {}", fields.pkgver));
  lines.push(format!("pkgrel = {}", fields.pkgrel));
  lines.push(format!("platform = {}", fields.platform));
  lines.push(format!("arch = {}", fields.arch));
  lines.push(format!("pkgmgr = {}", fields.pkgmgr));
  lines.push(format!("size = {}", fields.size));

  let mut output = lines.join("\n");
  output.push('\n');
  fs::write(target, output)?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::{BuiltFields, rewrite_manifest_for_build};
  use std::fs;

  fn rewrite(source_text: &str, pkgmgr: &str) -> String {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let source = tmp.path().join("manifest.src");
    let target = tmp.path().join("manifest.dst");
    fs::write(&source, source_text).expect("write source manifest");
    rewrite_manifest_for_build(
      &source,
      &target,
      &BuiltFields {
        pkgver: "1.2.3",
        pkgrel: "4",
        platform: "all",
        arch: "arm64",
        pkgmgr,
        size: 4096,
      },
    )
    .expect("rewrite manifest");
    fs::read_to_string(&target).expect("read target manifest")
  }

  #[test]
  fn replaces_owned_fields_and_keeps_author_fields() {
    let out = rewrite(
      "pkgname = hello\nplatform = old\narch = old\npkgver = 0.0.1\npkgrel = 1\nsize = 1\nbuilddate = 1\nuname = aarch64\n",
      "opkg",
    );
    assert!(out.contains("pkgname = hello"));
    assert!(out.contains("pkgver = 1.2.3"));
    assert!(out.contains("pkgrel = 4"));
    assert!(out.contains("platform = all"));
    assert!(out.contains("arch = arm64"));
    assert!(out.contains("pkgmgr = opkg"));
    assert!(out.contains("size = 4096"));
    assert!(out.contains("builddate = "));
    assert!(!out.contains("size = 1"));
    assert!(!out.contains("platform = old"));
  }

  #[test]
  fn keeps_uname_compat_gate() {
    let out = rewrite("pkgname = hello\nuname = aarch64\n", "apk");
    assert!(
      out.contains("uname = aarch64"),
      "uname is an author-declared compat gate and must survive into the artifact"
    );
  }

  #[test]
  fn keeps_source_availability_for_the_installed_manifest() {
    let out = rewrite("pkgname = hello\nsource_availability = partial\n", "pica");
    assert!(out.contains("source_availability = partial"), "{out}");
    assert!(!out.contains("visibility"), "the retired key must not be written: {out}");
  }

  #[test]
  fn writes_the_pica_backend_into_the_artifact() {
    let out = rewrite("pkgname = hello\ntargets = pica\n", "pica");
    assert!(out.contains("pkgmgr = pica"));
    assert!(!out.contains("targets"));
  }

  #[test]
  fn strips_build_only_targets_field() {
    let out = rewrite("pkgname = hello\ntargets = opkg\ntargets = apk\n", "apk");
    assert!(!out.contains("targets"), "targets is build-time only");
    assert!(out.contains("pkgmgr = apk"));
  }

  #[test]
  fn supports_legacy_pkgver_without_pkgrel() {
    let out = rewrite("pkgname = hello\npkgver = 0.1.0-7\nplatform = old\narch = old\n", "apk");
    assert!(out.contains("pkgver = 1.2.3"));
    assert!(out.contains("pkgrel = 4"));
    assert!(!out.contains("platform = old"));
  }
}
