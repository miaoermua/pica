use crate::app::{
  ensure_dirs, App, CliError, CliResult, E_CONFIG_INVALID, E_INDEX_INVALID, E_REPO_INVALID,
};
use crate::platform::detect_platform;
use crate::state::{ensure_json_object_field, read_json_file, write_json_atomic_pretty};
use crate::system::{fetch_url, has_command, opkg_has_package, opkg_update_ignore};
use pica_pkg_core::io::now_unix_secs;
use pica_pkg_core::repo::{is_supported_url, parse_repo_json};
use serde_json::{json, Value};

pub fn repos(app: &mut App) -> CliResult<()> {
  ensure_dirs(&app.paths)?;
  app.log_info("Synchronizing package databases...");

  let conf = read_json_file(&app.paths.conf_file)?;
  let repos = conf.get("repos").and_then(Value::as_array).cloned().unwrap_or_default();

  if repos.is_empty() {
    return Err(CliError::new(
      E_CONFIG_INVALID,
      format!("no repos configured in {}", app.paths.conf_file.display()),
    ));
  }

  let host_platform = detect_platform();
  let now = now_unix_secs();

  let mut index = read_json_file(&app.paths.index_file)?;
  if !index.is_object() {
    index = json!({"schema": 1, "repos": {}});
  }
  index["schema"] = json!(1);
  ensure_json_object_field(&mut index, "repos")?;

  for (repo_index, repo_entry) in repos.iter().enumerate() {
    let name = repo_entry.get("name").and_then(Value::as_str).unwrap_or("").trim();
    let url = repo_entry.get("url").and_then(Value::as_str).unwrap_or("").trim();
    let repo_platform = repo_entry.get("platform").and_then(Value::as_str).unwrap_or("").trim();

    if name.is_empty() {
      return Err(CliError::new(E_CONFIG_INVALID, format!("repo[{repo_index}] missing name")));
    }
    if url.is_empty() {
      return Err(CliError::new(E_CONFIG_INVALID, format!("repo[{repo_index}] missing url")));
    }

    app.log_info(format!("{name} downloading..."));
    let repo_json_url = format!("{}/repo.json", url.trim_end_matches('/'));
    let repo_raw = fetch_url(
      &repo_json_url,
      is_supported_url,
      app.options.fetch_timeout,
      app.options.fetch_retry,
      app.options.fetch_retry_delay,
    )?;
    let repo_text = String::from_utf8(repo_raw).map_err(|_| {
      CliError::new(E_REPO_INVALID, format!("{name}: repo.json is not valid UTF-8"))
    })?;
    let parsed_repo = parse_repo_json(&repo_text).map_err(|error| {
      CliError::new(
        E_REPO_INVALID,
        format!("{name}: repo.json failed strict schema/filename validation: {error}"),
      )
    })?;

    let repo_value = serde_json::to_value(&parsed_repo)
      .map_err(|error| CliError::new(E_REPO_INVALID, error.to_string()))?;

    let repo_cache_file = app.paths.repos_cache_dir.join(format!("{name}.json"));
    write_json_atomic_pretty(&repo_cache_file, &repo_value)?;

    let effective_platform =
      if repo_platform.is_empty() { host_platform.clone() } else { repo_platform.to_string() };

    let repo_obj = json!({
        "name": name,
        "url": url,
        "updated_at": now,
        "platform": effective_platform,
        "data": repo_value,
    });

    let index_repos = index
      .get_mut("repos")
      .and_then(Value::as_object_mut)
      .ok_or_else(|| CliError::new(E_INDEX_INVALID, "index repos is not object"))?;
    index_repos.insert(name.to_string(), repo_obj);

    app.log_info(format!("{name} updated"));
  }

  write_json_atomic_pretty(&app.paths.index_file, &index)?;

  check_index_dependencies(app);
  Ok(())
}

fn check_index_dependencies(app: &mut App) {
  if !has_command("opkg") {
    return;
  }

  opkg_update_ignore();

  let Ok(index) = read_json_file(&app.paths.index_file) else {
    return;
  };

  let mut missing_any = false;
  for dep in collect_declared_dependencies(&index) {
    if !opkg_has_package(&dep) {
      app.log_warn(format!("opkg feed missing dependency: {dep}"));
      missing_any = true;
    }
  }

  if missing_any {
    app.log_warn("Some declared dependencies are missing in opkg feeds");
  }
}

fn collect_declared_dependencies(index: &Value) -> Vec<String> {
  let mut values = Vec::new();

  let Some(repos) = index.get("repos").and_then(Value::as_object) else {
    return values;
  };

  for repo in repos.values() {
    let Some(packages) =
      repo.get("data").and_then(|data| data.get("packages")).and_then(Value::as_array)
    else {
      continue;
    };

    for package in packages {
      let manifest = package.get("manifest").unwrap_or(&Value::Null);
      append_dep_list(&mut values, manifest.get("base"));
      append_dep_list(&mut values, manifest.get("kmod"));
      append_dep_list(&mut values, manifest.get("app"));
    }
  }

  values.sort();
  values.dedup();
  values
}

fn append_dep_list(output: &mut Vec<String>, value: Option<&Value>) {
  match value {
    Some(Value::Array(values)) => {
      for entry in values {
        if let Some(text) = entry.as_str() {
          let trimmed = text.trim();
          if !trimmed.is_empty() {
            output.push(trimmed.to_string());
          }
        }
      }
    }
    Some(Value::String(text)) => {
      let trimmed = text.trim();
      if !trimmed.is_empty() {
        output.push(trimmed.to_string());
      }
    }
    _ => {}
  }
}

#[cfg(test)]
mod tests {
  use super::{append_dep_list, collect_declared_dependencies};
  use pretty_assertions::assert_eq;
  use serde_json::json;

  #[test]
  fn collect_declared_dependencies_merges_and_dedups() {
    let index = json!({
        "repos": {
            "r1": {
                "data": {
                    "packages": [
                        {"manifest": {"base": ["busybox", "dnsmasq"], "kmod": "kmod-tun", "app": ["luci-app-foo", "busybox"]}},
                        {"manifest": {"base": ["dnsmasq"], "app": "foo"}}
                    ]
                }
            },
            "r2": {
                "data": {
                    "packages": [
                        {"manifest": {"app": ["bar", "foo"], "kmod": ["kmod-tun", "kmod-usb"]}}
                    ]
                }
            }
        }
    });

    let deps = collect_declared_dependencies(&index);
    assert_eq!(
      deps,
      vec![
        "bar".to_string(),
        "busybox".to_string(),
        "dnsmasq".to_string(),
        "foo".to_string(),
        "kmod-tun".to_string(),
        "kmod-usb".to_string(),
        "luci-app-foo".to_string(),
      ]
    );
  }

  #[test]
  fn append_dep_list_ignores_empty_and_non_string() {
    let mut out = Vec::new();
    append_dep_list(&mut out, Some(&json!(["a", " ", 1, "b"])));
    append_dep_list(&mut out, Some(&json!(" c ")));
    append_dep_list(&mut out, Some(&json!(null)));
    append_dep_list(&mut out, Some(&json!(1)));

    assert_eq!(out, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
  }
}
