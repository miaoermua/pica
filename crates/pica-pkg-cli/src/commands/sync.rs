use crate::app::{
  App, CliError, CliResult, E_CONFIG_INVALID, E_INDEX_INVALID, E_REPO_INVALID, ensure_dirs,
};
use crate::i18n::{Key, Lang};
use crate::pkgmgr::PackageManagerKind;
use crate::platform::detect_platform;
use crate::source::{self, FetchPolicy};
use crate::state::{
  ensure_json_object_field, read_json_file, repo_package_sets, write_json_atomic_pretty,
};
use crate::system::fetch_url;
use pica_pkg_core::io::now_unix_secs;
use pica_pkg_core::repo::{parse_backend_index, parse_entry_descriptor, parse_repo_json};
use serde_json::{Value, json};

/// Backends a source may advertise. Kept as text so an error can list them.
const SUPPORTED_BACKENDS: &str = "opkg, apk, pica";

pub fn repos(app: &mut App) -> CliResult<()> {
  ensure_dirs(&app.paths)?;
  app.info(Key::SyncStarted);

  let conf = read_json_file(&app.paths.conf_file)?;
  let missing_repos = || {
    CliError::new(
      E_CONFIG_INVALID,
      app.args(Key::SyncNoRepos, &[&app.paths.conf_file.display().to_string()]),
    )
  };
  let repos = match conf.get("repos") {
    Some(value) => value.as_array().cloned().ok_or_else(|| {
      CliError::new(
        E_CONFIG_INVALID,
        app.args(Key::SyncReposNotArray, &[&app.paths.conf_file.display().to_string()]),
      )
    })?,
    None => return Err(missing_repos()),
  };

  if repos.is_empty() {
    return Err(missing_repos());
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
    let allow_insecure_http =
      repo_entry.get("allow_insecure_http").and_then(Value::as_bool).unwrap_or(false);

    if name.is_empty() {
      let index = repo_index.to_string();
      return Err(CliError::new(E_CONFIG_INVALID, app.args(Key::SyncRepoMissingName, &[&index])));
    }
    if url.is_empty() {
      let index = repo_index.to_string();
      return Err(CliError::new(E_CONFIG_INVALID, app.args(Key::SyncRepoMissingUrl, &[&index])));
    }

    // The root is what every address below it is derived from, and what the
    // policy is anchored to; a root the client would refuse to fetch from is a
    // configuration error, not a repository one.
    let policy = FetchPolicy::source(allow_insecure_http);
    policy.check(url).map_err(|err| {
      CliError::new(E_CONFIG_INVALID, app.args(Key::SyncRootRejected, &[name, url, &err.message]))
    })?;

    let wanted = configured_backends(app.lang, repo_index, name, repo_entry)?;

    app.info_args(Key::SyncDownloading, &[name]);

    let effective_platform =
      if repo_platform.is_empty() { host_platform.clone() } else { repo_platform.to_string() };

    let body = match wanted {
      Some(backends) => sync_from_descriptor(app, name, url, &backends, policy, now)?,
      // No `backends` key is a configuration written for the schema-2 layout.
      // It keeps working exactly as it did; a source that only ships the entry
      // descriptor is reached through the descriptor instead, and that attempt
      // is reported only if both fail.
      None => match sync_legacy(app, name, url, policy, now) {
        Ok(body) => body,
        Err(legacy_error) => {
          let Ok(declared) = all_declared_backends(app, url, policy) else {
            return Err(legacy_error);
          };
          app.warn_args(Key::SyncFallback, &[name, &join_backends(&declared)]);
          let mut body = sync_from_descriptor(app, name, url, &declared, policy, now)?;
          body["legacy_error"] = json!(legacy_error.message);
          body
        }
      },
    };

    let repo_obj = json!({
        "name": name,
        "url": url,
        "updated_at": now,
        "platform": effective_platform,
        "allow_insecure_http": allow_insecure_http,
        "kind": body["kind"].clone(),
        "backends": body["backends"].clone(),
        "entry": body["entry"].clone(),
        "indexes": body["indexes"].clone(),
        "data": body["data"].clone(),
        "legacy_error": body["legacy_error"].clone(),
    });

    let repo_cache_file = app.paths.repos_cache_dir.join(format!("{name}.json"));
    write_json_atomic_pretty(&repo_cache_file, &repo_obj)?;

    let index_repos = index
      .get_mut("repos")
      .and_then(Value::as_object_mut)
      .ok_or_else(|| CliError::new(E_INDEX_INVALID, "index repos is not object"))?;
    index_repos.insert(name.to_string(), repo_obj);

    app.info_args(Key::SyncUpdated, &[name]);
  }

  write_json_atomic_pretty(&app.paths.index_file, &index)?;

  check_index_dependencies(app);
  Ok(())
}

/// The backend combination this source was configured for, in the order it was
/// written. `None` means the configuration predates the key and the source is
/// read the old way.
fn configured_backends(
  lang: Lang,
  repo_index: usize,
  name: &str,
  repo_entry: &Value,
) -> CliResult<Option<Vec<String>>> {
  let Some(value) = repo_entry.get("backends") else {
    return Ok(None);
  };
  let list = value.as_array().ok_or_else(|| {
    let index = repo_index.to_string();
    CliError::new(E_CONFIG_INVALID, lang.args(Key::SyncBackendsNotArray, &[&index, name]))
  })?;

  let mut backends = Vec::new();
  for item in list {
    let text = item.as_str().map_or("", str::trim);
    let kind = PackageManagerKind::parse(text).ok_or_else(|| {
      let index = repo_index.to_string();
      CliError::new(
        E_CONFIG_INVALID,
        lang.args(
          Key::SyncUnsupportedBackend,
          &[&index, name, &format!("{text:?}"), SUPPORTED_BACKENDS],
        ),
      )
    })?;
    if !backends.iter().any(|existing| existing == kind.as_str()) {
      backends.push(kind.as_str().to_string());
    }
  }

  if backends.is_empty() {
    let index = repo_index.to_string();
    return Err(CliError::new(
      E_CONFIG_INVALID,
      lang.args(Key::SyncBackendsEmpty, &[&index, name]),
    ));
  }

  Ok(Some(backends))
}

/// Fetch one of a source's files, refusing anything the policy would not have
/// fetched on its own.
fn fetch_text(
  app: &App,
  name: &str,
  what: &str,
  url: &str,
  policy: FetchPolicy,
) -> CliResult<String> {
  let raw = fetch_url(
    url,
    policy,
    app.options.fetch_timeout,
    app.options.fetch_retry,
    app.options.fetch_retry_delay,
  )
  .map_err(|err| {
    CliError::new(err.code, app.args(Key::SyncFetchFailed, &[name, what, url, &err.message]))
  })?;

  String::from_utf8(raw)
    .map_err(|_| CliError::new(E_REPO_INVALID, app.args(Key::SyncNotUtf8, &[name, what, url])))
}

/// The schema-2 layout: one `repo.json` describing every package of a source.
fn sync_legacy(
  app: &App,
  name: &str,
  root: &str,
  policy: FetchPolicy,
  now: u64,
) -> CliResult<Value> {
  let repo_url = source::join_child(root, "repo.json")?;
  let text = fetch_text(app, name, "repo.json", &repo_url, policy)?;

  let parsed = parse_repo_json(&text).map_err(|error| {
    CliError::new(
      E_REPO_INVALID,
      app.args(Key::SyncRepoInvalid, &[name, &repo_url, &error.to_string()]),
    )
  })?;

  let data = serde_json::to_value(&parsed)
    .map_err(|error| CliError::new(E_REPO_INVALID, error.to_string()))?;

  Ok(json!({
    "kind": "legacy",
    "backends": Vec::<String>::new(),
    "entry": Value::Null,
    "indexes": Value::Null,
    "data": data,
    "updated_at": now,
  }))
}

/// The entry-descriptor layout: `index.json` names a path per backend, and each
/// of those paths is fetched as its own index.
fn sync_from_descriptor(
  app: &App,
  name: &str,
  root: &str,
  wanted: &[String],
  policy: FetchPolicy,
  now: u64,
) -> CliResult<Value> {
  let entry_url = source::join_child(root, "index.json")?;
  let text = fetch_text(app, name, "index.json", &entry_url, policy)?;

  let descriptor = parse_entry_descriptor(&text).map_err(|error| {
    CliError::new(
      E_REPO_INVALID,
      app.args(Key::SyncEntryInvalid, &[name, &entry_url, &error.to_string()]),
    )
  })?;

  let declared: Vec<&str> = descriptor.managers.keys().map(String::as_str).collect();

  // A backend the user asked for but the source does not publish is a hard
  // error: installing through the other one would be a different package.
  let missing: Vec<&str> = wanted
    .iter()
    .map(String::as_str)
    .filter(|mgr| !descriptor.managers.contains_key(*mgr))
    .collect();
  if !missing.is_empty() {
    return Err(CliError::new(
      E_REPO_INVALID,
      app.args(
        Key::SyncMissingBackend,
        &[
          name,
          &join_backends(&missing.iter().map(|m| (*m).to_string()).collect::<Vec<_>>()),
          &join_backends(&declared.iter().map(|m| (*m).to_string()).collect::<Vec<_>>()),
        ],
      ),
    ));
  }

  let mut indexes = serde_json::Map::new();
  for mgr in wanted {
    let path = &descriptor.managers[mgr].path;
    // Relative to the descriptor's own location, never to a guess about the
    // deployment (GitHub raw, a Worker and a self-hosted prefix all differ).
    let url = source::join_sibling(&entry_url, path)?;
    let text = fetch_text(app, name, &format!("{mgr} index"), &url, policy)?;

    let parsed = parse_backend_index(&text, mgr).map_err(|error| {
      CliError::new(
        E_REPO_INVALID,
        app.args(Key::SyncIndexInvalid, &[name, mgr, &url, &error.to_string()]),
      )
    })?;

    indexes.insert(
      mgr.clone(),
      json!({
        "url": url,
        "schema": parsed.schema,
        "mgr": parsed.mgr,
        "generated_at": parsed.generated_at,
        "fetched_at": now,
        "packages": parsed.packages,
      }),
    );
  }

  Ok(json!({
    "kind": "entry",
    "backends": wanted,
    "entry": {
      "url": entry_url,
      "schema": descriptor.schema,
      "name": descriptor.name,
      "description": descriptor.description,
    },
    "indexes": indexes,
    "data": Value::Null,
    "updated_at": now,
  }))
}

fn all_declared_backends(app: &App, root: &str, policy: FetchPolicy) -> CliResult<Vec<String>> {
  let entry_url = source::join_child(root, "index.json")?;
  let text = fetch_text(app, "source", "index.json", &entry_url, policy)?;
  let descriptor = parse_entry_descriptor(&text).map_err(|error| {
    CliError::new(
      E_REPO_INVALID,
      app.args(Key::SyncEntryInvalid, &["source", &entry_url, &error.to_string()]),
    )
  })?;
  Ok(descriptor.managers.keys().cloned().collect())
}

fn join_backends(backends: &[String]) -> String {
  if backends.is_empty() { "none".to_string() } else { backends.join(", ") }
}

fn check_index_dependencies(app: &mut App) {
  let Ok(index) = read_json_file(&app.paths.index_file) else {
    return;
  };

  let (manager_name, checked) = {
    let Ok(package_manager) = app.package_manager() else {
      app.log_warn("skip dependency check: no supported package manager found");
      return;
    };
    let checked = package_manager.update_index().and_then(|()| {
      let mut missing = Vec::new();
      for dep in collect_declared_dependencies(&index) {
        if !package_manager.package_exists(&dep)? {
          missing.push(dep);
        }
      }
      Ok(missing)
    });
    (package_manager.kind().as_str(), checked)
  };

  // The check is a warning, not a gate, so a backend that cannot be read costs a
  // warning of its own. It must not cost anything else: reporting a feed as
  // missing a dependency it was never asked about would read as a fact about
  // the repository, on a host whose repositories are simply out of reach.
  let missing = match checked {
    Ok(missing) => missing,
    Err(err) => {
      app.log_warn(format!("skip dependency check: {}", err.message));
      return;
    }
  };

  for dep in &missing {
    app.log_warn(format!("{manager_name} feed missing dependency: {dep}"));
  }
  if !missing.is_empty() {
    app.log_warn(format!("Some declared dependencies are missing in {manager_name} feeds"));
  }
}

fn collect_declared_dependencies(index: &Value) -> Vec<String> {
  let mut values = Vec::new();

  let Some(repos) = index.get("repos").and_then(Value::as_object) else {
    return values;
  };

  for repo in repos.values() {
    // A source read through its entry descriptor keeps one index per backend;
    // a schema-2 source keeps all of them in `data`. Both are read here so a
    // dependency is never invisible just because of the layout it arrived in.
    for packages in repo_package_sets(repo) {
      for package in packages {
        let manifest = package.get("manifest").unwrap_or(&Value::Null);
        append_dep_list(&mut values, manifest.get("base"));
        append_dep_list(&mut values, manifest.get("kmod"));
        append_dep_list(&mut values, manifest.get("app"));
      }
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
