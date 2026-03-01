use pica_pkg_core::error::PicaResult;
use pica_pkg_core::io::now_unix_secs;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

pub(crate) fn rewrite_manifest_for_build(
  source: &Path,
  target: &Path,
  pkgver: &str,
  pkgrel: &str,
  platform: &str,
  arch: &str,
) -> PicaResult<()> {
  let content = fs::read_to_string(source)?;
  let mut lines = Vec::new();
  let remove_keys: HashSet<&str> =
    ["builddate", "size", "platform", "arch", "pkgver", "pkgrel", "uname"].into_iter().collect();

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

    let key = raw_key.trim();
    if remove_keys.contains(key) {
      continue;
    }
    lines.push(raw_line.to_string());
  }

  lines.push(format!("builddate = {}", now_unix_secs()));
  lines.push(format!("pkgver = {pkgver}"));
  lines.push(format!("pkgrel = {pkgrel}"));
  lines.push(format!("platform = {platform}"));
  lines.push(format!("arch = {arch}"));

  let mut output = lines.join("\n");
  output.push('\n');
  fs::write(target, output)?;
  Ok(())
}

pub(crate) fn append_manifest_kv(path: &Path, key: &str, value: &str) -> PicaResult<()> {
  let mut content = fs::read_to_string(path)?;
  if !content.ends_with('\n') {
    content.push('\n');
  }
  content.push_str(key);
  content.push_str(" = ");
  content.push_str(value);
  content.push('\n');
  fs::write(path, content)?;
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::rewrite_manifest_for_build;
  use std::fs;

  #[test]
  fn rewrite_manifest_for_build_replaces_runtime_fields() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().to_path_buf();
    let source = root.join("manifest.src");
    let target = root.join("manifest.dst");

    fs::write(
      &source,
      "pkgname = hello\nplatform = old\narch = old\npkgver = 0.0.1\npkgrel = 1\nsize = 1\nbuilddate = 1\n",
    )
    .expect("write source manifest");

    rewrite_manifest_for_build(&source, &target, "1.2.3", "4", "all", "arm64")
      .expect("rewrite manifest");

    let out = fs::read_to_string(&target).expect("read target manifest");
    assert!(out.contains("pkgname = hello"));
    assert!(out.contains("pkgver = 1.2.3"));
    assert!(out.contains("pkgrel = 4"));
    assert!(out.contains("platform = all"));
    assert!(out.contains("arch = arm64"));
    assert!(out.contains("builddate = "));
    assert!(!out.contains("size = 1"));
    assert!(!out.contains("platform = old"));
  }

  #[test]
  fn rewrite_manifest_supports_legacy_pkgver_without_pkgrel() {
    let tmp = tempfile::tempdir().expect("create temp dir");
    let root = tmp.path().to_path_buf();
    let source = root.join("manifest.src");
    let target = root.join("manifest.dst");

    fs::write(&source, "pkgname = hello\npkgver = 0.1.0-7\nplatform = old\narch = old\n")
      .expect("write source manifest");

    rewrite_manifest_for_build(&source, &target, "0.1.0", "7", "all", "all")
      .expect("rewrite manifest");

    let out = fs::read_to_string(&target).expect("read target manifest");
    assert!(out.contains("pkgver = 0.1.0"));
    assert!(out.contains("pkgrel = 7"));
    assert!(out.contains("platform = all"));
    assert!(out.contains("arch = all"));
    assert!(!out.contains("platform = old"));
  }
}
