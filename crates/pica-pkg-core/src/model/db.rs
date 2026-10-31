//! `/var/lib/pica/db.json`: what is installed, and what each install wrote.

use crate::manifest::{get_first, get_scalar};
use crate::version::pkgver_cmp_key;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// The whole database.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Db {
  #[serde(default = "schema_one")]
  pub schema: u64,
  /// Records by `pkgname`, in name order.
  #[serde(default)]
  pub installed: BTreeMap<String, DbEntry>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

fn schema_one() -> u64 {
  1
}

/// One installed package.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DbEntry {
  /// The artifact's manifest, with `source` and `pkgmgr` overwritten by the
  /// install. Its keys are the package's own, so it stays a JSON value.
  #[serde(default)]
  pub manifest: Value,
  /// The artifact path for an artifact install, or the backend name for a feed
  /// install, which has no artifact.
  #[serde(default)]
  pub pkgfile: String,
  /// What the install wrote. A feed install records package names here, since
  /// the backend owns the real file list.
  #[serde(default)]
  pub files: Vec<String>,
  /// Directories the install created.
  #[serde(default)]
  pub dirs: Vec<String>,
  #[serde(default)]
  pub installed_at: u64,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

impl DbEntry {
  /// The first value of a manifest field, or empty.
  #[must_use]
  pub fn field(&self, key: &str) -> String {
    get_first(&self.manifest, key)
  }

  /// The single value of a manifest field, or empty.
  #[must_use]
  pub fn scalar(&self, key: &str) -> String {
    get_scalar(&self.manifest, key)
  }

  /// `pkgver-pkgrel` as the index and the transaction header print it.
  #[must_use]
  pub fn version(&self) -> String {
    pkgver_cmp_key(&self.field("pkgver"), &self.field("pkgrel"))
  }

  /// The name a selector matches: `appname`, else the recorded `pkgname`, else
  /// the key the record is stored under.
  #[must_use]
  pub fn appname_or(&self, key: &str) -> String {
    let appname = self.field("appname");
    if !appname.is_empty() {
      return appname;
    }
    let pkgname = self.field("pkgname");
    if pkgname.is_empty() { key.to_string() } else { pkgname }
  }

  /// Absolute paths this install wrote. Feed records list package names in
  /// `files`, and those are left out.
  pub fn owned_paths(&self) -> impl Iterator<Item = &str> {
    // `/` is what a device writes; the platform check covers a target root
    // elsewhere, which is how the tests run.
    self
      .files
      .iter()
      .map(String::as_str)
      .filter(|path| path.starts_with('/') || std::path::Path::new(path).is_absolute())
  }

  /// Adds paths not already named, in the order given.
  pub fn merge_paths(&mut self, files: &[String], dirs: &[String]) {
    for path in files {
      if !self.files.contains(path) {
        self.files.push(path.clone());
      }
    }
    for path in dirs {
      if !self.dirs.contains(path) {
        self.dirs.push(path.clone());
      }
    }
  }
}

impl Db {
  /// The record whose key, `appname` or `pkgname` is `name`, on `branch` when
  /// one is given. Returns the key it is stored under.
  #[must_use]
  pub fn find(&self, name: &str, branch: &str) -> Option<(&str, &DbEntry)> {
    self.installed.iter().map(|(key, entry)| (key.as_str(), entry)).find(|(key, entry)| {
      let named = *key == name || entry.field("appname") == name || entry.field("pkgname") == name;
      named && (branch.is_empty() || entry.field("branch") == branch)
    })
  }

  /// Who else records `path`: the first record other than `except` that lists
  /// it among the paths it wrote.
  #[must_use]
  pub fn owner_of(&self, path: &str, except: &str) -> Option<&str> {
    self
      .installed
      .iter()
      .find(|(key, entry)| *key != except && entry.owned_paths().any(|owned| owned == path))
      .map(|(key, _)| key.as_str())
  }
}

#[cfg(test)]
mod tests {
  use super::{Db, DbEntry};
  use pretty_assertions::assert_eq;
  use serde_json::json;

  #[test]
  fn a_database_keeps_fields_it_does_not_know() {
    let text = json!({
      "schema": 1,
      "future": true,
      "installed": {
        "demo": {
          "manifest": {"pkgname": "demo", "pkgver": "1.0", "pkgrel": "2"},
          "pkgfile": "/tmp/demo.pkg.tar.gz",
          "files": ["/etc/demo"],
          "dirs": [],
          "installed_at": 3,
          "note": "kept"
        }
      }
    });
    let db: Db = serde_json::from_value(text.clone()).expect("parse");
    assert_eq!(db.installed["demo"].version(), "1.0-2");
    assert_eq!(serde_json::to_value(&db).expect("serialize"), text);
  }

  #[test]
  fn a_record_written_without_some_fields_still_reads() {
    let db: Db =
      serde_json::from_value(json!({"installed": {"x": {"manifest": {}}}})).expect("parse");
    assert_eq!(db.schema, 1);
    assert_eq!(db.installed["x"], DbEntry { manifest: json!({}), ..DbEntry::default() });
  }

  #[test]
  fn a_record_is_found_by_key_appname_or_pkgname_and_branch() {
    let db: Db = serde_json::from_value(json!({"installed": {
      "demo@alice": {"manifest": {"appname": "demo", "branch": "stable"}},
      "other": {"manifest": {"pkgname": "other"}}
    }}))
    .expect("parse");
    assert_eq!(db.find("demo", "").map(|(key, _)| key), Some("demo@alice"));
    assert_eq!(db.find("demo", "stable").map(|(key, _)| key), Some("demo@alice"));
    assert_eq!(db.find("demo", "edge"), None);
    assert_eq!(db.find("other", "").map(|(key, _)| key), Some("other"));
  }

  #[test]
  fn only_paths_count_as_owned_and_the_owner_is_another_record() {
    let db: Db = serde_json::from_value(json!({"installed": {
      "a": {"files": ["/etc/shared", "luci-app-a"]},
      "b": {"files": ["/etc/shared"]}
    }}))
    .expect("parse");
    assert_eq!(db.owner_of("/etc/shared", "a"), Some("b"));
    assert_eq!(db.owner_of("luci-app-a", "b"), None);
  }
}
