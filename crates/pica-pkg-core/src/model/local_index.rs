//! `/var/lib/pica/index.json`: the sync cache, one entry per configured source.

use crate::repo::Package;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// What `pica -S` wrote last.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LocalIndex {
  #[serde(default)]
  pub schema: u64,
  /// Required: a cache without it was not written by a sync.
  pub repos: BTreeMap<String, SyncedRepo>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

/// One synced source.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncedRepo {
  #[serde(default)]
  pub name: String,
  /// The source root.
  #[serde(default)]
  pub url: String,
  /// When this source was synced, in Unix seconds.
  #[serde(default)]
  pub updated_at: u64,
  #[serde(default)]
  pub allow_insecure_http: bool,
  #[serde(default)]
  pub backends: Vec<String>,
  #[serde(default)]
  pub indexes: BTreeMap<String, SyncedIndex>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

/// One backend index of a synced source.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncedIndex {
  /// Entries were validated when the source was synced. One that no longer
  /// reads as a package (written by hand, or by another release) is left out
  /// on its own rather than making the whole cache unreadable.
  #[serde(default, deserialize_with = "readable_packages")]
  pub packages: Vec<Package>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

fn readable_packages<'de, D>(deserializer: D) -> Result<Vec<Package>, D::Error>
where
  D: serde::Deserializer<'de>,
{
  let values = Vec::<Value>::deserialize(deserializer)?;
  Ok(values.into_iter().filter_map(|value| serde_json::from_value(value).ok()).collect())
}

impl SyncedRepo {
  /// Every package this source published, across its backends.
  pub fn packages(&self) -> impl Iterator<Item = &Package> {
    self.indexes.values().flat_map(|index| index.packages.iter())
  }
}

impl LocalIndex {
  /// Every package of every synced source, with the source it came from.
  pub fn packages(&self) -> impl Iterator<Item = (&str, &SyncedRepo, &Package)> {
    self
      .repos
      .iter()
      .flat_map(|(name, repo)| repo.packages().map(move |package| (name.as_str(), repo, package)))
  }

  /// The oldest sync time among the sources, or `None` with none synced.
  #[must_use]
  pub fn oldest_sync(&self) -> Option<u64> {
    self.repos.values().map(|repo| repo.updated_at).min()
  }
}

#[cfg(test)]
mod tests {
  use super::LocalIndex;
  use pretty_assertions::assert_eq;
  use serde_json::json;

  #[test]
  fn every_backend_of_every_source_is_listed() {
    let package = |name: &str, mgr: &str| {
      json!({
        "pkgname": name, "pkgver": "1", "pkgrel": "1", "pkgmgr": mgr, "platform": "all",
        "arch": "all", "filename": "f", "sha256": "0"
      })
    };
    let index: LocalIndex = serde_json::from_value(json!({
      "schema": 1,
      "repos": {
        "a": {"updated_at": 5, "indexes": {"pica": {"packages": [package("x", "pica")]}}},
        "b": {"updated_at": 3, "indexes": {
          "opkg": {"packages": [package("y", "opkg")]},
          "apk": {"packages": [package("z", "apk")]}
        }}
      }
    }))
    .expect("parse");

    let names: Vec<_> =
      index.packages().map(|(repo, _, pkg)| format!("{repo}/{}", pkg.pkgname)).collect();
    assert_eq!(names, ["a/x", "b/z", "b/y"]);
    assert_eq!(index.oldest_sync(), Some(3));
  }

  #[test]
  fn an_entry_that_is_not_a_package_costs_only_itself() {
    let index: LocalIndex = serde_json::from_value(json!({
      "repos": {"a": {"indexes": {"pica": {"packages": [
        {"manifest": {}},
        {"pkgname": "x", "pkgver": "1", "pkgrel": "1", "pkgmgr": "pica", "platform": "all",
         "arch": "all", "filename": "f", "sha256": "0"}
      ]}}}}
    }))
    .expect("parse");
    assert_eq!(index.packages().count(), 1);
  }

  #[test]
  fn a_cache_without_sources_is_not_a_sync_cache() {
    assert!(serde_json::from_value::<LocalIndex>(json!({"schema": 1})).is_err());
  }
}
