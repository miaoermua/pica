use crate::error::{PicaError, PicaResult};
use crate::selector::Selector;
use crate::version::pkgver_cmp_key;
use serde_json::{Map, Value};
use std::fs;
use std::path::Path;

/// Separator for backend-scoped overrides, e.g. `app@apk = demo-v3`.
pub const BACKEND_SCOPE: char = '@';

/// Build targets declared in a source manifest (`targets = opkg`).
///
/// Accepts both the repeatable form and a comma-separated list so that the
/// field behaves like every other list field in the manifest.
#[must_use]
pub fn split_targets(values: &[String]) -> Vec<String> {
  let mut out: Vec<String> = Vec::new();
  for value in values {
    for item in value.split(',') {
      let item = item.trim();
      if item.is_empty() || out.iter().any(|seen| seen == item) {
        continue;
      }
      out.push(item.to_string());
    }
  }
  out
}

fn scoped_key(key: &str, backend: &str) -> String {
  format!("{key}{BACKEND_SCOPE}{backend}")
}

#[derive(Debug, Clone)]
pub struct Manifest {
  pub value: Value,
}

impl Manifest {
  /// # Errors
  /// Returns an error if the file cannot be read or parsed.
  pub fn from_file(path: impl AsRef<Path>) -> PicaResult<Self> {
    let content = fs::read_to_string(path)?;
    Self::from_text(&content)
  }

  /// # Errors
  /// This function is infallible in practice but returns `PicaResult` for API consistency.
  pub fn from_text(content: &str) -> PicaResult<Self> {
    let mut map = Map::<String, Value>::new();

    for raw_line in content.lines() {
      let line = trim_comment(raw_line);
      if line.is_empty() {
        continue;
      }

      let Some((raw_key, raw_value)) = line.split_once('=') else {
        continue;
      };

      let key = raw_key.trim();
      let value = raw_value.trim();
      if key.is_empty() {
        continue;
      }

      match map.get_mut(key) {
        Some(existing) if existing.is_array() => {
          if let Some(arr) = existing.as_array_mut() {
            arr.push(Value::String(value.to_string()));
          }
        }
        Some(existing) => {
          let previous = existing.take();
          *existing = Value::Array(vec![previous, Value::String(value.to_string())]);
        }
        None => {
          map.insert(key.to_string(), Value::String(value.to_string()));
        }
      }
    }

    Ok(Self { value: Value::Object(map) })
  }

  #[must_use]
  pub fn get_first(&self, key: &str) -> String {
    get_first(&self.value, key)
  }

  #[must_use]
  pub fn get_scalar(&self, key: &str) -> String {
    get_scalar(&self.value, key)
  }

  #[must_use]
  pub fn get_array(&self, key: &str) -> Vec<String> {
    get_array(&self.value, key)
  }

  #[must_use]
  pub fn has(&self, key: &str) -> bool {
    self.value.get(key).is_some()
  }

  /// Backend-scoped lookup: `<key>@<backend>` wins when present, otherwise `<key>`.
  ///
  /// A present-but-empty scoped value means "explicitly nothing for this backend",
  /// so presence is checked rather than emptiness.
  #[must_use]
  pub fn get_first_for(&self, key: &str, backend: &str) -> String {
    get_first_for(&self.value, key, backend)
  }

  #[must_use]
  pub fn get_array_for(&self, key: &str, backend: &str) -> Vec<String> {
    get_array_for(&self.value, key, backend)
  }

  #[must_use]
  pub fn targets(&self) -> Vec<String> {
    split_targets(&self.get_array("targets"))
  }

  #[must_use]
  pub fn pkgver_display(&self) -> String {
    let pkgver = self.get_first("pkgver");
    let pkgrel = self.get_first("pkgrel");
    pkgver_cmp_key(&pkgver, &pkgrel)
  }

  #[must_use]
  pub fn canonical_selector(&self, fallback_pkgname: &str) -> String {
    let appname = self.get_first("appname");
    let appname = if appname.is_empty() { fallback_pkgname.to_string() } else { appname };

    let selector = Selector {
      raw: String::new(),
      norm: String::new(),
      appname,
      branch: self.get_first("branch"),
    };

    selector.to_canonical_string()
  }

  #[must_use]
  pub fn has_type(&self, target: &str) -> bool {
    self.get_array("type").into_iter().any(|item| item == target)
  }

  #[must_use]
  pub fn with_selector_defaults(mut self, fallback_pkgname: &str) -> Self {
    if let Some(obj) = self.value.as_object_mut() {
      let _ = fallback_pkgname;
      for key in ["branch", "protocol"] {
        if obj.get(key).is_none() {
          obj.insert(key.to_string(), Value::String(String::new()));
        }
      }
    }
    self
  }

  /// # Errors
  /// Returns an error if the key is missing or its value is empty.
  pub fn require_non_empty(&self, key: &str) -> PicaResult<String> {
    let value = self.get_first(key);
    if value.is_empty() {
      Err(PicaError::msg(format!("manifest missing {key}")))
    } else {
      Ok(value)
    }
  }

  #[must_use]
  pub fn to_pretty_text(&self) -> String {
    let mut lines = Vec::new();
    if let Some(obj) = self.value.as_object() {
      for (key, value) in obj {
        match value {
          Value::Array(values) => {
            for item in values {
              lines.push(format!("{key} = {}", value_to_text(item)));
            }
          }
          _ => lines.push(format!("{key} = {}", value_to_text(value))),
        }
      }
    }
    if lines.is_empty() { String::new() } else { format!("{}\n", lines.join("\n")) }
  }

  /// # Errors
  /// Returns an error if JSON serialization fails.
  pub fn to_string(&self) -> PicaResult<String> {
    Ok(serde_json::to_string(&self.value)?)
  }
}

#[must_use]
pub fn get_first(value: &Value, key: &str) -> String {
  let Some(entry) = value.get(key) else {
    return String::new();
  };
  match entry {
    Value::Null => String::new(),
    Value::Array(items) => items.first().map_or_else(String::new, value_to_text),
    _ => value_to_text(entry),
  }
}

#[must_use]
pub fn get_scalar(value: &Value, key: &str) -> String {
  let Some(entry) = value.get(key) else {
    return String::new();
  };
  match entry {
    Value::Array(_) | Value::Null => String::new(),
    _ => value_to_text(entry),
  }
}

#[must_use]
pub fn get_array(value: &Value, key: &str) -> Vec<String> {
  let Some(entry) = value.get(key) else {
    return Vec::new();
  };
  match entry {
    Value::Array(values) => values.iter().map(value_to_text).collect(),
    Value::Null => Vec::new(),
    _ => vec![value_to_text(entry)],
  }
}

/// [`Manifest::get_first_for`] for callers holding a raw JSON manifest, such as
/// database records.
#[must_use]
pub fn get_first_for(value: &Value, key: &str, backend: &str) -> String {
  get_first(value, &scoped_lookup_key(value, key, backend))
}

/// [`Manifest::get_array_for`] for callers holding a raw JSON manifest.
#[must_use]
pub fn get_array_for(value: &Value, key: &str, backend: &str) -> Vec<String> {
  get_array(value, &scoped_lookup_key(value, key, backend))
}

fn scoped_lookup_key(value: &Value, key: &str, backend: &str) -> String {
  let scoped = scoped_key(key, backend);
  if value.get(&scoped).is_some() { scoped } else { key.to_string() }
}

fn trim_comment(input: &str) -> &str {
  let without_comment = input.split('#').next().unwrap_or("");
  without_comment.trim()
}

fn value_to_text(value: &Value) -> String {
  match value {
    Value::String(text) => text.clone(),
    Value::Bool(flag) => {
      if *flag {
        "true".to_string()
      } else {
        "false".to_string()
      }
    }
    Value::Number(number) => number.to_string(),
    Value::Null => String::new(),
    other => other.to_string(),
  }
}

#[cfg(test)]
mod tests {
  use super::Manifest;

  #[test]
  fn parse_manifest_repeatable() {
    let text = r"
        pkgname = hello
        app = hello
        app = luci-app-hello
        # comment
        ";

    let manifest = Manifest::from_text(text).expect("parse manifest");
    assert_eq!(manifest.get_first("pkgname"), "hello");
    assert_eq!(manifest.get_array("app"), vec!["hello", "luci-app-hello"]);
  }

  #[test]
  fn parse_manifest_selector() {
    let text = r"
        pkgname = hello
        appname = hello
        branch = stable
        ";
    let manifest = Manifest::from_text(text).expect("parse manifest");
    assert_eq!(manifest.canonical_selector("hello"), "hello(stable)");
  }

  #[test]
  fn backend_scope_overrides_shared_key() {
    let manifest = Manifest::from_text(
      r"
        app = demo
        app = luci-app-demo
        app@apk = demo-v3
        base = luci-base
        base@apk =
        ",
    )
    .expect("parse manifest");

    assert_eq!(manifest.get_array_for("app", "opkg"), vec!["demo", "luci-app-demo"]);
    assert_eq!(manifest.get_array_for("app", "apk"), vec!["demo-v3"]);
    assert_eq!(manifest.get_array_for("base", "opkg"), vec!["luci-base"]);
    // An empty scoped value shadows the shared list; callers drop empty entries.
    let apk_base_raw = manifest.get_array_for("base", "apk");
    let apk_base: Vec<&str> =
      apk_base_raw.iter().map(String::as_str).filter(|item| !item.is_empty()).collect();
    assert!(apk_base.is_empty());
    assert_eq!(manifest.get_first_for("app", "apk"), "demo-v3");
    assert_eq!(manifest.get_first_for("app", "pica"), "demo");
  }

  #[test]
  fn targets_accepts_repeated_and_comma_forms() {
    let repeated = Manifest::from_text("targets = opkg\ntargets = apk\n").expect("parse");
    assert_eq!(repeated.targets(), vec!["opkg", "apk"]);

    let comma = Manifest::from_text("targets = apk,opkg,apk\n").expect("parse");
    assert_eq!(comma.targets(), vec!["apk", "opkg"]);

    let mixed = Manifest::from_text("targets = opkg, apk\ntargets = pica\n").expect("parse");
    assert_eq!(mixed.targets(), vec!["opkg", "apk", "pica"]);

    assert!(Manifest::from_text("pkgname = x").expect("parse").targets().is_empty());
  }
}
