use crate::error::{PicaError, PicaResult};
use crate::selector::Selector;
use crate::version::pkgver_cmp_key;
use serde_json::{Map, Value};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Manifest {
    pub value: Value,
}

impl Manifest {
    pub fn from_file(path: impl AsRef<Path>) -> PicaResult<Self> {
        let content = fs::read_to_string(path)?;
        Self::from_text(&content)
    }

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

        Ok(Self {
            value: Value::Object(map),
        })
    }

    pub fn get_first(&self, key: &str) -> String {
        get_first(&self.value, key)
    }

    pub fn get_scalar(&self, key: &str) -> String {
        get_scalar(&self.value, key)
    }

    pub fn get_array(&self, key: &str) -> Vec<String> {
        get_array(&self.value, key)
    }

    pub fn pkgver_display(&self) -> String {
        let pkgver = self.get_first("pkgver");
        let pkgrel = self.get_first("pkgrel");
        pkgver_cmp_key(&pkgver, &pkgrel)
    }

    pub fn canonical_selector(&self, fallback_pkgname: &str) -> String {
        let appname = self.get_first("appname").chars().collect::<String>();

        let appname = if appname.is_empty() {
            let pkgname = self.get_first("pkgname");
            if pkgname.is_empty() {
                fallback_pkgname.to_string()
            } else {
                pkgname
            }
        } else {
            appname
        };

        let selector = Selector {
            raw: String::new(),
            norm: String::new(),
            appname,
            branch: self.get_first("branch"),
        };

        selector.to_canonical_string()
    }

    pub fn has_type(&self, target: &str) -> bool {
        self.get_array("type")
            .into_iter()
            .any(|item| item == target)
    }

    pub fn with_source_default(mut self, source: &str) -> Self {
        if self.value.get("source").is_none() {
            if let Some(obj) = self.value.as_object_mut() {
                obj.insert("source".to_string(), Value::String(source.to_string()));
            }
        }
        self
    }

    pub fn with_selector_defaults(mut self, fallback_pkgname: &str) -> Self {
        if let Some(obj) = self.value.as_object_mut() {
            if obj.get("appname").is_none() {
                obj.insert(
                    "appname".to_string(),
                    Value::String(fallback_pkgname.to_string()),
                );
            }
            for key in ["branch", "protocol"] {
                if obj.get(key).is_none() {
                    obj.insert(key.to_string(), Value::String(String::new()));
                }
            }
        }
        self
    }

    pub fn require_non_empty(&self, key: &str) -> PicaResult<String> {
        let value = self.get_first(key);
        if value.is_empty() {
            Err(PicaError::msg(format!("manifest missing {key}")))
        } else {
            Ok(value)
        }
    }

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
        if lines.is_empty() {
            String::new()
        } else {
            format!("{}\n", lines.join("\n"))
        }
    }

    pub fn to_string(&self) -> PicaResult<String> {
        Ok(serde_json::to_string(&self.value)?)
    }
}

pub fn get_first(value: &Value, key: &str) -> String {
    let Some(entry) = value.get(key) else {
        return String::new();
    };
    match entry {
        Value::Null => String::new(),
        Value::Array(items) => items.first().map(value_to_text).unwrap_or_else(String::new),
        _ => value_to_text(entry),
    }
}

pub fn get_scalar(value: &Value, key: &str) -> String {
    let Some(entry) = value.get(key) else {
        return String::new();
    };
    match entry {
        Value::Array(_) | Value::Null => String::new(),
        _ => value_to_text(entry),
    }
}

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

fn trim_comment(input: &str) -> &str {
    let without_comment = input.split('#').next().unwrap_or("");
    without_comment.trim()
}

fn value_to_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.to_string(),
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
        let text = r#"
        pkgname = hello
        app = hello
        app = luci-app-hello
        # comment
        "#;

        let manifest = Manifest::from_text(text).expect("parse manifest");
        assert_eq!(manifest.get_first("pkgname"), "hello");
        assert_eq!(manifest.get_array("app"), vec!["hello", "luci-app-hello"]);
    }

    #[test]
    fn parse_manifest_selector() {
        let text = r#"
        pkgname = hello
        appname = hello
        branch = stable
        "#;
        let manifest = Manifest::from_text(text).expect("parse manifest");
        assert_eq!(manifest.canonical_selector("hello"), "hello(stable)");
    }
}
