//! `/etc/pica/pica.json`: the operator's settings.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The configuration file. Each field is optional on disk; the accessors say
/// what a missing value means.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
  /// The interface language of Pica itself, as written.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub language: Option<String>,
  /// Language-pack code used for `luci-i18n-<app>-<i18n>`, as written.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub i18n: Option<String>,
  /// The configured backend. `null`, a missing key and an empty string all
  /// mean auto-detection.
  #[serde(default)]
  pub pkgmgr: Option<String>,
  /// Raw, so a value that is not a boolean can be reported instead of being
  /// read as one.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub apk_force_non_repository: Option<Value>,
  /// Raw, because each entry is validated where it is used and the error has
  /// to name the entry's position.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub repos: Option<Value>,
  #[serde(flatten)]
  pub extra: Map<String, Value>,
}

impl Config {
  /// The configured backend, when one is configured at all.
  #[must_use]
  pub fn pkgmgr(&self) -> Option<&str> {
    self.pkgmgr.as_deref().map(str::trim).filter(|value| !value.is_empty())
  }
}

#[cfg(test)]
mod tests {
  use super::Config;
  use pretty_assertions::assert_eq;
  use serde_json::json;

  #[test]
  fn an_empty_or_null_backend_means_auto_detection() {
    for value in [json!(null), json!(""), json!("  ")] {
      let conf: Config = serde_json::from_value(json!({ "pkgmgr": value })).expect("parse");
      assert_eq!(conf.pkgmgr(), None);
    }
    let conf: Config = serde_json::from_value(json!({"pkgmgr": "apk"})).expect("parse");
    assert_eq!(conf.pkgmgr(), Some("apk"));
  }

  #[test]
  fn unknown_settings_survive_a_round_trip() {
    let text = json!({"language": "zh-CN", "repos": [], "pkgmgr": null, "custom": 1});
    let conf: Config = serde_json::from_value(text.clone()).expect("parse");
    assert_eq!(serde_json::to_value(&conf).expect("serialize"), text);
  }
}
