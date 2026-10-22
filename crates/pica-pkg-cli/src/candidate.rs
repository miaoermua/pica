use crate::app::{App, CliError, CliResult, E_CONFIG_INVALID, E_INDEX_INVALID, ensure_dirs};
use crate::pkgmgr::PackageManagerKind;
use crate::platform::{ArchVerdict, HostArches, check_arch, detect_os, detect_package_arches};
use crate::state::{read_json_file, repo_package_sets};
use pica_pkg_core::selector::Selector;
use pica_pkg_core::version::pkgver_cmp_key;
use serde_json::Value;
use std::cell::OnceCell;

pub(crate) fn find_pica_candidates_in_index(
  app: &App,
  selector: &str,
) -> CliResult<Vec<RepoCandidate>> {
  // Which arches this host accepts is asked at most once, and only when an
  // entry makes the answer matter: the question drives the package manager,
  // and a search that matches nothing (or only `all`) must not touch it.
  let host_arches = OnceCell::new();
  find_candidates_for_host(app, selector, &host_arches)
}

/// Whether an entry declaring `pkg_arch` can be offered on this host.
///
/// The line is drawn where the install-time gate draws it, and for the same
/// reason: only a real disagreement excludes an entry. `all` says the artifact
/// runs anywhere, and a missing declaration says nothing at all — neither can
/// be read as a disagreement. A family match (`Related`) and a host that could
/// not state its own arch (`Unverified`) are kept here and warned about at
/// precheck time; dropping them silently would turn "cannot tell" into "not
/// for you".
///
/// The host is asked only for a declaration that needs an answer, so an index
/// of `all` entries leaves the package manager alone.
fn arch_offerable(app: &App, host_arches: &OnceCell<HostArches>, pkg_arch: &str) -> bool {
  if pkg_arch.is_empty() || pkg_arch == "all" {
    return true;
  }

  let host_arches = host_arches.get_or_init(|| detect_package_arches(app));
  !matches!(check_arch(pkg_arch, host_arches), ArchVerdict::Mismatch)
}

/// A clause for the "not found" error that says which backends each synced
/// source actually offers.
///
/// The distinction matters: a package that exists only in a backend this source
/// does not publish is a different problem from a misspelled name, and saying
/// which backends were searched keeps the client from quietly answering with
/// the wrong artifact.
#[must_use]
pub(crate) fn not_found_hint(app: &App) -> String {
  let backend = app
    .package_manager()
    .map_or_else(|_| "unknown".to_string(), |manager| manager.kind().as_str().to_string());

  let Ok(index) = read_json_file(&app.paths.index_file) else {
    return format!(" (backend {backend}; no synced sources, run -S first)");
  };
  let Some(repos) = index.get("repos").and_then(Value::as_object) else {
    return format!(" (backend {backend}; no synced sources, run -S first)");
  };

  if repos.is_empty() {
    return format!(" (backend {backend}; no synced sources, run -S first)");
  }

  let mut parts = Vec::new();
  for (name, repo) in repos {
    let provided = match repo.get("backends").and_then(Value::as_array) {
      Some(list) if !list.is_empty() => {
        list.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")
      }
      // A schema-2 source carries every backend in one file.
      _ => "every backend (schema 2)".to_string(),
    };
    parts.push(format!("{name} provides {provided}"));
  }

  format!(" (backend {backend}; {})", parts.join("; "))
}

/// The search itself, judged against a host the caller names.
///
/// Split from the entry point so a test can state the host instead of asking
/// the machine it happens to run on.
fn find_candidates_for_host(
  app: &App,
  selector: &str,
  host_arches: &OnceCell<HostArches>,
) -> CliResult<Vec<RepoCandidate>> {
  ensure_dirs(&app.paths)?;

  let index = read_json_file(&app.paths.index_file)?;
  let parsed = Selector::parse(selector).map_err(|err| CliError::new(E_CONFIG_INVALID, err))?;
  let host_os = detect_os();
  let host_backend = app.backend();

  let mut out = Vec::new();

  let repos = index
    .get("repos")
    .and_then(Value::as_object)
    .ok_or_else(|| CliError::new(E_INDEX_INVALID, "missing index: run 'pica -S' first"))?;

  for (repo_name, repo_entry) in repos {
    let repo_url = repo_entry.get("url").and_then(Value::as_str).unwrap_or("").to_string();
    let allow_insecure_http =
      repo_entry.get("allow_insecure_http").and_then(Value::as_bool).unwrap_or(false);
    // The entry descriptor layout keeps one index per backend; the schema-2
    // layout keeps them all in `data`. Both are searched, and the backend each
    // entry declares still decides whether it can be installed here.
    let package_sets = repo_package_sets(repo_entry);
    if package_sets.is_empty() {
      continue;
    }

    for pkg in package_sets.iter().flat_map(|packages| packages.iter()) {
      let pkgname = pkg.get("pkgname").and_then(Value::as_str).unwrap_or("").to_string();
      let appname = pkg.get("appname").and_then(Value::as_str).unwrap_or(&pkgname).to_string();
      let branch = pkg.get("branch").and_then(Value::as_str).unwrap_or("").to_string();
      let pkgver = pkg.get("pkgver").and_then(Value::as_str).unwrap_or("").to_string();
      let pkgrel = pkg.get("pkgrel").and_then(Value::as_str).unwrap_or("").to_string();
      let protocol = pkg.get("protocol").and_then(Value::as_str).unwrap_or("").to_string();
      let pkg_url = pkg
        .get("url")
        .or_else(|| pkg.get("origin"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
      let luci_url = pkg.get("luci_url").and_then(Value::as_str).unwrap_or("").to_string();
      let luci_desc = pkg.get("luci_desc").and_then(Value::as_str).unwrap_or("").to_string();
      let pkgmgr = pkg.get("pkgmgr").and_then(Value::as_str).unwrap_or("").to_string();
      let pkgdesc = pkg
        .get("pkgdesc")
        .and_then(Value::as_str)
        .or_else(|| {
          pkg.get("manifest").and_then(|manifest| manifest.get("pkgdesc")).and_then(Value::as_str)
        })
        .unwrap_or("")
        .to_string();
      let pkg_platform = pkg.get("platform").and_then(Value::as_str).unwrap_or("").to_string();
      let pkg_arch = pkg.get("arch").and_then(Value::as_str).unwrap_or("").to_string();
      let size = index_bytes(pkg, "size");
      let csize = index_bytes(pkg, "csize");
      let filename = pkg.get("filename").and_then(Value::as_str).unwrap_or("").to_string();
      let download_url = pkg.get("download_url").and_then(Value::as_str).map(ToString::to_string);
      let min_pica = pkg.get("pica").and_then(Value::as_str).map(ToString::to_string);
      let sha256 = pkg.get("sha256").and_then(Value::as_str).unwrap_or("").to_string();

      if appname != parsed.appname {
        continue;
      }
      if !parsed.branch.is_empty() && branch != parsed.branch {
        continue;
      }

      let pkg_os = pkg.get("os").and_then(Value::as_str).unwrap_or("").to_string();

      let os_match = pkg_os.is_empty() || pkg_os == "all" || pkg_os == host_os;
      if !os_match {
        continue;
      }

      // Backend identity: a `pica` package ships a payload Pica installs itself
      // and needs nothing from the host, so it matches anywhere. `opkg` and
      // `apk` packages need their own backend to be the one in use here.
      let backend_match = match PackageManagerKind::parse(&pkgmgr) {
        Some(kind) => !kind.requires_system_binary() || host_backend == kind,
        None => false,
      };
      if !backend_match {
        continue;
      }

      // An index publishes one entry per architecture, so this is what keeps a
      // device from picking a sibling's artifact and failing the precheck.
      if !arch_offerable(app, host_arches, &pkg_arch) {
        continue;
      }

      out.push(RepoCandidate {
        cmpver: pkgver_cmp_key(&pkgver, &pkgrel),
        repo: repo_name.clone(),
        url: repo_url.clone(),
        appname,
        branch,
        pkgver,
        pkgrel,
        protocol,
        pkg_url,
        luci_url,
        luci_desc,
        pkgmgr,
        pkgdesc,
        os: pkg_os,
        platform: pkg_platform,
        arch: pkg_arch,
        size,
        csize,
        filename,
        download_url,
        min_pica,
        sha256,
        pkgname,
        allow_insecure_http,
      });
    }
  }

  Ok(out)
}

/// A byte figure the index records: `size` (installed volume) or `csize`
/// (compressed artifact).
///
/// `None` means the index does not record the figure. A recorded 0 is returned
/// as 0 and left for the caller to interpret — it is not folded into `None`
/// here, because "the index says zero" and "the index says nothing" are
/// different facts even when neither can be shown.
fn index_bytes(pkg: &Value, key: &str) -> Option<u64> {
  match pkg.get(key) {
    Some(Value::Number(number)) => number.as_u64(),
    Some(Value::String(text)) => text.trim().parse::<u64>().ok(),
    _ => None,
  }
}

#[derive(Debug, Clone)]
pub(crate) struct RepoCandidate {
  pub(crate) cmpver: String,
  pub(crate) repo: String,
  pub(crate) url: String,
  pub(crate) appname: String,
  pub(crate) branch: String,
  pub(crate) pkgver: String,
  pub(crate) pkgrel: String,
  pub(crate) protocol: String,
  pub(crate) pkg_url: String,
  pub(crate) luci_url: String,
  pub(crate) luci_desc: String,
  pub(crate) pkgmgr: String,
  pub(crate) pkgdesc: String,
  pub(crate) os: String,
  pub(crate) platform: String,
  pub(crate) arch: String,
  pub(crate) size: Option<u64>,
  pub(crate) csize: Option<u64>,
  pub(crate) filename: String,
  pub(crate) download_url: Option<String>,
  pub(crate) min_pica: Option<String>,
  pub(crate) sha256: String,
  pub(crate) pkgname: String,
  /// The source's own answer to "may a public host be reached over plain
  /// HTTP". It travels with the candidate so the download is judged by the
  /// policy of the source that offered it, not by a global setting.
  pub(crate) allow_insecure_http: bool,
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::app::{FeedPolicy, JsonMode, OptionalDeps, Options, Paths};
  use serde_json::json;
  use std::fs;
  use std::path::Path;

  /// An app whose state lives under `root`, with the `pica` backend forced so a
  /// test does not depend on which package managers the host happens to have.
  fn test_app(root: &Path) -> App {
    let etc = root.join("etc");
    let state = root.join("state");
    let paths = Paths {
      etc_dir: etc.clone(),
      conf_file: etc.join("pica.json"),
      state_dir: state.clone(),
      db_file: state.join("db.json"),
      index_file: state.join("index.json"),
      cache_dir: state.join("cache"),
      repos_cache_dir: state.join("cache").join("repos"),
      report_file: state.join("install-report.json"),
      lock_file: state.join("db.lck"),
    };
    let options = Options {
      json_mode: JsonMode::None,
      non_interactive: true,
      feed_policy: FeedPolicy::PackagedOnly,
      fetch_timeout: 1,
      fetch_retry: 0,
      fetch_retry_delay: 0,
      pkgmgr: None,
      optional_deps: OptionalDeps::None,
    };

    let mut app = App::new(paths, options);
    app.select_package_manager(PackageManagerKind::Pica.as_str()).expect("force the pica backend");
    app
  }

  /// One index entry, with only the fields the search reads spelled out.
  fn entry(arch: &str) -> Value {
    json!({
      "pkgname": "demo",
      "appname": "demo",
      "pkgver": "1.0.0",
      "pkgrel": "1",
      "platform": "all",
      "arch": arch,
      "pkgmgr": "pica",
      "os": "all",
      "filename": format!("demo-1.0.0-1-{arch}.pkg.tar.gz"),
      "sha256": "0".repeat(64),
    })
  }

  fn write_index(app: &App, packages: &[Value]) {
    let index = json!({
      "repos": {
        "main": {
          "url": "https://repo.invalid/main",
          "data": { "packages": packages },
        }
      }
    });
    fs::create_dir_all(&app.paths.state_dir).expect("create state dir");
    fs::write(&app.paths.index_file, index.to_string()).expect("write index");
  }

  /// One synced entry-descriptor source: one index per backend, exactly as
  /// `-S` stores it.
  fn write_backend_indexes(app: &App, indexes: &[(&str, Vec<Value>)]) {
    let mut map = serde_json::Map::new();
    for (mgr, packages) in indexes {
      map.insert(
        (*mgr).to_string(),
        json!({
          "url": format!("https://repo.invalid/main/{mgr}/index.json"),
          "schema": 1,
          "mgr": mgr,
          "packages": packages,
        }),
      );
    }

    let index = json!({
      "repos": {
        "main": {
          "url": "https://repo.invalid/main",
          "kind": "entry",
          "backends": indexes.iter().map(|(mgr, _)| *mgr).collect::<Vec<_>>(),
          "indexes": map,
        }
      }
    });
    fs::create_dir_all(&app.paths.state_dir).expect("create state dir");
    fs::write(&app.paths.index_file, index.to_string()).expect("write index");
  }

  fn backend_entry(pkgname: &str, version: &str, pkgmgr: &str) -> Value {
    json!({
      "pkgname": pkgname,
      "appname": pkgname,
      "pkgver": version,
      "pkgrel": "1",
      "platform": "all",
      "arch": "all",
      "pkgmgr": pkgmgr,
      "os": "all",
      "filename": format!("{pkgname}-{version}-1-all-{pkgmgr}.pkg.tar.gz"),
      "sha256": "0".repeat(64),
    })
  }

  fn kernel(uname: &str) -> HostArches {
    HostArches::Kernel { uname: uname.to_string() }
  }

  #[test]
  fn the_same_name_in_two_backends_stays_two_packages() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let app = test_app(tmp.path());
    write_backend_indexes(
      &app,
      &[
        ("opkg", vec![backend_entry("demo", "2.0.0", "opkg")]),
        ("pica", vec![backend_entry("demo", "1.0.0", "pica")]),
      ],
    );

    // This host runs the pica backend, so the opkg artifact of the same name is
    // a different package and must not be offered — not even as a fallback.
    let found =
      find_candidates_for_host(&app, "demo", &OnceCell::from(kernel("x86_64"))).expect("search");

    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].pkgmgr, "pica");
    assert_eq!(found[0].pkgver, "1.0.0");
  }

  #[test]
  fn every_index_of_a_source_is_searched() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let app = test_app(tmp.path());
    write_backend_indexes(
      &app,
      &[
        ("opkg", vec![backend_entry("other", "1.0.0", "opkg")]),
        ("pica", vec![backend_entry("demo", "1.0.0", "pica")]),
      ],
    );

    let found =
      find_candidates_for_host(&app, "demo", &OnceCell::from(kernel("x86_64"))).expect("search");

    assert_eq!(found.len(), 1, "a match in any backend's index is a match");
    assert_eq!(found[0].pkgname, "demo");
  }

  #[test]
  fn a_source_states_whether_its_public_host_may_be_plain_http() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let app = test_app(tmp.path());
    let mut index = json!({
      "repos": {
        "main": {
          "url": "http://mirror.lan/main",
          "allow_insecure_http": true,
          "data": { "packages": [backend_entry("demo", "1.0.0", "pica")] },
        }
      }
    });
    fs::create_dir_all(&app.paths.state_dir).expect("create state dir");
    fs::write(&app.paths.index_file, index.to_string()).expect("write index");

    let found =
      find_candidates_for_host(&app, "demo", &OnceCell::from(kernel("x86_64"))).expect("search");
    assert!(found[0].allow_insecure_http, "the source's own answer travels with the candidate");

    index["repos"]["main"]["allow_insecure_http"] = json!(false);
    fs::write(&app.paths.index_file, index.to_string()).expect("rewrite index");
    let found =
      find_candidates_for_host(&app, "demo", &OnceCell::from(kernel("x86_64"))).expect("search");
    assert!(!found[0].allow_insecure_http);
  }

  #[test]
  fn an_arch_this_device_cannot_run_is_not_offered() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let app = test_app(tmp.path());
    write_index(&app, &[entry("x86_64"), entry("mipsel_24kc")]);

    let found =
      find_candidates_for_host(&app, "demo", &OnceCell::from(kernel("x86_64"))).expect("search");

    assert_eq!(found.len(), 1, "one of the two cannot run here");
    assert_eq!(found[0].arch, "x86_64");
  }

  #[test]
  fn an_arch_of_all_is_offered_on_any_host() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let app = test_app(tmp.path());
    write_index(&app, &[entry("all")]);

    let found = find_candidates_for_host(&app, "demo", &OnceCell::from(kernel("mipsel_24kc")))
      .expect("search");

    assert_eq!(found.len(), 1, "`all` is the artifact saying it runs anywhere");
    assert_eq!(found[0].arch, "all");
  }

  #[test]
  fn a_host_that_cannot_say_what_it_is_offers_everything() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let app = test_app(tmp.path());
    write_index(&app, &[entry("x86_64"), entry("mipsel_24kc")]);

    let found =
      find_candidates_for_host(&app, "demo", &OnceCell::from(HostArches::Unknown)).expect("search");

    assert_eq!(found.len(), 2, "an unverified host must not exclude anything");
  }

  #[test]
  fn a_single_entry_index_is_offered_as_it_was() {
    let tmp = tempfile::tempdir().expect("temp dir");
    let app = test_app(tmp.path());
    write_index(&app, &[entry("all")]);

    let found =
      find_candidates_for_host(&app, "demo", &OnceCell::from(kernel("x86_64"))).expect("search");

    assert_eq!(found.len(), 1);
    let candidate = &found[0];
    assert_eq!(candidate.pkgname, "demo");
    assert_eq!(candidate.arch, "all");
    assert_eq!(candidate.filename, "demo-1.0.0-1-all.pkg.tar.gz");
  }
}
