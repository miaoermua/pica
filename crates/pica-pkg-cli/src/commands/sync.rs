use crate::app::{App, CliError, CliResult, E_CONFIG_INVALID, E_REPO_INVALID, ensure_dirs};
use crate::i18n::Key;
use crate::i18n::tr;
use crate::pkgmgr::PackageManagerKind;
use crate::platform::detect_platform;
use crate::source::{self, FetchPolicy};
use crate::state::{Transaction, read_json_file};
use crate::system::fetch_url;
use pica_pkg_core::io::now_unix_secs;
use pica_pkg_core::layout::SUPPORTED_PKGMGRS;
use pica_pkg_core::model::local_index::LocalIndex;
use pica_pkg_core::repo::{parse_backend_index_skipping, parse_entry_descriptor};
use serde_json::{Value, json};

/// Everything one sync publishes, built before a single byte is written.
///
/// Building first is what makes the sync transactional: a source that fails to
/// fetch or parse cannot leave its own cache, another source's cache or the
/// shared index half updated, because nothing has been written yet.
pub struct Plan {
  index: Value,
  caches: Vec<(String, Value)>,
}

impl Plan {
  /// Adds every file this plan publishes to `tx`: one cache per source first,
  /// then the index that references them.
  ///
  /// # Errors
  /// Returns an error if a file's current contents cannot be read or staged.
  pub fn stage(self, app: &App, tx: &mut Transaction) -> CliResult<()> {
    for (name, value) in self.caches {
      let path = app.paths.repos_cache_dir.join(format!("{name}.json"));
      tx.stage_json(app, &path, &value)?;
    }
    tx.stage_json(app, &app.paths.index_file, &self.index)
  }
}

pub fn repos(app: &mut App) -> CliResult<()> {
  ensure_dirs(&app.paths)?;
  app.info(Key::SyncStarted);

  let repos = configured_repos(app)?;
  let plan = build(app, &repos)?;

  let mut tx = Transaction::new();
  plan.stage(app, &mut tx)?;
  tx.commit(app)?;

  check_index_dependencies(app);
  Ok(())
}

/// The `repos[]` array from the configuration.
fn configured_repos(app: &App) -> CliResult<Vec<Value>> {
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

  Ok(repos)
}

/// Fetches, parses and validates every configured source without writing
/// anything. The caller commits the result, all at once.
///
/// # Errors
/// Returns the first source's error, describing what was wrong with it.
pub fn build(app: &mut App, repos: &[Value]) -> CliResult<Plan> {
  let host_platform = detect_platform();
  let now = now_unix_secs();
  let mut index = json!({"schema": 1, "repos": {}});
  let mut caches = Vec::new();

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

    let wanted = configured_backends(app, repo_index, name, repo_entry)?;

    app.info_args(Key::SyncDownloading, &[name]);

    let effective_platform =
      if repo_platform.is_empty() { host_platform.clone() } else { repo_platform.to_string() };

    let repo_obj =
      sync_source(app, name, url, &wanted, now, allow_insecure_http, &effective_platform)?;

    index["repos"]
      .as_object_mut()
      .ok_or_else(|| CliError::new(E_CONFIG_INVALID, tr(Key::IndexReposNotObject, &[])))?
      .insert(name.to_string(), repo_obj.clone());
    caches.push((name.to_string(), repo_obj));

    app.info_args(Key::SyncUpdated, &[name]);
  }

  Ok(Plan { index, caches })
}

/// The backends one source publishes, read from its entry descriptor.
///
/// `setup` has to know this before it can write a `backends` list, and it must
/// read the source exactly the way the sync will, so both go through these
/// rules: the same `index.json`, the same policy, the same wording on failure.
/// The sync that follows reads the descriptor again; that second read is what
/// decides what gets written, so nothing is trusted from the first one.
///
/// # Errors
/// Returns the fetch or parse error, described the way a sync describes it.
pub fn declared_backends(
  app: &App,
  name: &str,
  root: &str,
  policy: FetchPolicy,
) -> CliResult<Vec<String>> {
  let entry_url = source::join_child(root, "index.json")?;
  let text = fetch_text(app, name, "index.json", &entry_url, policy)?;

  let descriptor = parse_entry_descriptor(&text).map_err(|error| {
    CliError::new(
      E_REPO_INVALID,
      app.args(Key::SyncEntryInvalid, &[name, &entry_url, &error.to_string()]),
    )
  })?;
  Ok(descriptor.managers.keys().cloned().collect())
}

/// The backend combination this source was configured for, in the order it was
/// written. There is no default: the source protocol cannot say which of its
/// backends this host wants, and guessing would sync indexes nobody asked for.
fn configured_backends(
  app: &App,
  repo_index: usize,
  name: &str,
  repo_entry: &Value,
) -> CliResult<Vec<String>> {
  let index = repo_index.to_string();
  let Some(value) = repo_entry.get("backends") else {
    return Err(CliError::new(
      E_CONFIG_INVALID,
      app.args(Key::SyncBackendsMissing, &[&index, name]),
    ));
  };
  let list = value.as_array().ok_or_else(|| {
    CliError::new(E_CONFIG_INVALID, app.args(Key::SyncBackendsNotArray, &[&index, name]))
  })?;

  let mut backends = Vec::new();
  for item in list {
    let text = item.as_str().map_or("", str::trim);
    let kind = PackageManagerKind::parse(text).ok_or_else(|| {
      CliError::new(
        E_CONFIG_INVALID,
        app.args(
          Key::SyncUnsupportedBackend,
          &[&index, name, &format!("{text:?}"), &SUPPORTED_PKGMGRS.join(", ")],
        ),
      )
    })?;
    if !backends.iter().any(|existing| existing == kind.as_str()) {
      backends.push(kind.as_str().to_string());
    }
  }

  if backends.is_empty() {
    return Err(CliError::new(E_CONFIG_INVALID, app.args(Key::SyncBackendsEmpty, &[&index, name])));
  }

  Ok(backends)
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

/// Reads one source through its entry descriptor: `index.json` names a path per
/// backend, and each of those paths is fetched as its own index.
///
/// `index.json` is the only entry. A source that does not serve it is reported
/// as such; there is no second format to fall back to.
fn sync_source(
  app: &mut App,
  name: &str,
  root: &str,
  wanted: &[String],
  now: u64,
  allow_insecure_http: bool,
  platform: &str,
) -> CliResult<Value> {
  // The root was checked before this was called; the policy below is the same
  // one, so every child address is held to the same rule as its root.
  let policy = FetchPolicy::source(allow_insecure_http);
  let entry_url = source::join_child(root, "index.json")?;
  let text = fetch_text(app, name, "index.json", &entry_url, policy)?;

  let descriptor = parse_entry_descriptor(&text).map_err(|error| {
    CliError::new(
      E_REPO_INVALID,
      app.args(Key::SyncEntryInvalid, &[name, &entry_url, &error.to_string()]),
    )
  })?;

  let declared: Vec<String> = descriptor.managers.keys().cloned().collect();

  // A backend the user asked for but the source does not publish is a hard
  // error: installing through the other one would be a different package.
  let missing: Vec<String> =
    wanted.iter().filter(|mgr| !descriptor.managers.contains_key(*mgr)).cloned().collect();
  if !missing.is_empty() {
    return Err(CliError::new(
      E_REPO_INVALID,
      app.args(
        Key::SyncMissingBackend,
        &[name, &join_backends(&missing), &join_backends(&declared)],
      ),
    ));
  }

  let mut indexes = serde_json::Map::new();
  for mgr in wanted {
    let path = &descriptor.managers[mgr].path;
    // Relative to the descriptor's own location, never to a guess about the
    // deployment (GitHub raw, a Worker and a self-hosted prefix all differ).
    let url = source::join_sibling(&entry_url, path)?;
    let text = fetch_text(app, name, &tr(Key::IndexLabel, &[&mgr]), &url, policy)?;

    let (parsed, skipped) = parse_backend_index_skipping(&text, mgr).map_err(|error| {
      CliError::new(
        E_REPO_INVALID,
        app.args(Key::SyncIndexInvalid, &[name, mgr, &url, &error.to_string()]),
      )
    })?;
    for package in skipped {
      app.warn_args(
        Key::SyncPackageSkipped,
        &[name, mgr, package.field, &format!("{:?}", package.value)],
      );
    }

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
    "name": name,
    "url": root,
    "updated_at": now,
    "platform": platform,
    "allow_insecure_http": allow_insecure_http,
    "backends": wanted,
    "entry": {
      "url": entry_url,
      "schema": descriptor.schema,
      "name": descriptor.name,
      "description": descriptor.description,
    },
    "indexes": indexes,
  }))
}

fn join_backends(backends: &[String]) -> String {
  if backends.is_empty() { "none".to_string() } else { backends.join(", ") }
}

fn check_index_dependencies(app: &mut App) {
  let Ok(index) = crate::state::read_local_index(&app.paths) else {
    return;
  };

  let (manager_name, checked) = {
    let Ok(package_manager) = app.package_manager() else {
      app.log_warn(tr(Key::SyncDepCheckNoManager, &[]));
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
      app.log_warn(tr(Key::SyncDepCheckSkipped, &[&err.message]));
      return;
    }
  };

  for dep in &missing {
    app.log_warn(tr(Key::SyncFeedMissingDep, &[&manager_name, &dep]));
  }
  if !missing.is_empty() {
    app.log_warn(tr(Key::SyncFeedMissingSome, &[&manager_name]));
  }
}

fn collect_declared_dependencies(index: &LocalIndex) -> Vec<String> {
  let mut values = Vec::new();
  for (_, _, package) in index.packages() {
    let manifest = package.manifest.as_ref().unwrap_or(&Value::Null);
    append_dep_list(&mut values, manifest.get("base"));
    append_dep_list(&mut values, manifest.get("kmod"));
    append_dep_list(&mut values, manifest.get("app"));
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
  use super::{LocalIndex, append_dep_list, collect_declared_dependencies};
  use pretty_assertions::assert_eq;
  use serde_json::json;

  #[test]
  fn collect_declared_dependencies_merges_and_dedups() {
    // A sync cache holds whole, validated entries; only their manifests matter here.
    let pkg = |name: &str, manifest: serde_json::Value| {
      json!({
        "pkgname": name, "pkgver": "1", "pkgrel": "1", "pkgmgr": "pica", "platform": "all",
        "arch": "all", "filename": "f", "sha256": "0", "manifest": manifest
      })
    };
    let index = json!({
        "repos": {
            "r1": {
                "indexes": {
                    "pica": {"packages": [
                        pkg("a", json!({"base": ["busybox", "dnsmasq"], "kmod": "kmod-tun", "app": ["luci-app-foo", "busybox"]})),
                        pkg("b", json!({"base": ["dnsmasq"], "app": "foo"}))
                    ]}
                }
            },
            "r2": {
                "indexes": {
                    "apk": {"packages": [pkg("c", json!({"base": "busybox", "kmod": ["kmod-tun"]}))]},
                    "opkg": {"packages": [pkg("d", json!({"base": "rpcd"}))]}
                }
            }
        }
    });

    let index: LocalIndex = serde_json::from_value(index).expect("a sync cache");
    assert_eq!(
      collect_declared_dependencies(&index),
      vec![
        "busybox".to_string(),
        "dnsmasq".to_string(),
        "foo".to_string(),
        "kmod-tun".to_string(),
        "luci-app-foo".to_string(),
        "rpcd".to_string(),
      ]
    );
  }

  #[test]
  fn append_dep_list_accepts_arrays_and_strings() {
    let mut out = Vec::new();
    append_dep_list(&mut out, Some(&json!([" a ", ""])));
    append_dep_list(&mut out, Some(&json!(" b ")));
    append_dep_list(&mut out, Some(&json!(42)));
    assert_eq!(out, vec!["a".to_string(), "b".to_string()]);
  }
}
