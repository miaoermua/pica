use crate::state::read_json_file;
use crate::system;
use crate::{
  ensure_dirs, App, CliError, CliResult, FeedPolicy, Manifest, Selector, E_CONFIG_INVALID,
  E_INDEX_INVALID, E_IO, E_POLICY_INVALID, E_RUNTIME,
};
use pica_pkg_core::io::{
  copy_dir_recursive as core_copy_dir_recursive, make_temp_dir as core_make_temp_dir,
};
use pica_pkg_core::version::pkgver_cmp_key;
use serde_json::{json, Value};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) fn find_pica_candidates_in_index(
  app: &App,
  selector: &str,
) -> CliResult<Vec<RepoCandidate>> {
  ensure_dirs(&app.paths)?;

  let index = read_json_file(&app.paths.index_file)?;
  let parsed = Selector::parse(selector).map_err(|err| CliError::new(E_CONFIG_INVALID, err))?;
  let host_os = detect_os();

  let mut out = Vec::new();

  let repos = index
    .get("repos")
    .and_then(Value::as_object)
    .ok_or_else(|| CliError::new(E_INDEX_INVALID, "missing index: run 'pica -S' first"))?;

  for (repo_name, repo_entry) in repos {
    let repo_url = repo_entry.get("url").and_then(Value::as_str).unwrap_or("").to_string();
    let Some(packages) =
      repo_entry.get("data").and_then(|data| data.get("packages")).and_then(Value::as_array)
    else {
      continue;
    };

    for pkg in packages {
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
      let size = match pkg.get("size") {
        Some(Value::Number(number)) => number.as_u64(),
        Some(Value::String(text)) => text.trim().parse::<u64>().ok(),
        _ => None,
      };
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
        filename,
        download_url,
        min_pica,
        sha256,
        pkgname,
      });
    }
  }

  Ok(out)
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
  pub(crate) filename: String,
  pub(crate) download_url: Option<String>,
  pub(crate) min_pica: Option<String>,
  pub(crate) sha256: String,
  pub(crate) pkgname: String,
}

pub(crate) fn required_manifest_field(manifest: &Manifest, key: &str) -> CliResult<String> {
  let value = manifest.get_first(key);
  if value.is_empty() {
    Err(CliError::new(E_CONFIG_INVALID, format!("manifest missing {key}")))
  } else {
    Ok(value)
  }
}

pub(crate) fn canonicalize_display(path: &Path) -> String {
  fs::canonicalize(path).map_or_else(|_| path.display().to_string(), |p| p.display().to_string())
}

pub(crate) fn normalize_uname(value: &str) -> String {
  match value {
    "x86_64" => "amd64".to_string(),
    "aarch64" => "arm64".to_string(),
    other => other.to_string(),
  }
}

pub(crate) fn detect_opkg_arches() -> Vec<String> {
  let Ok(output) = Command::new("opkg").arg("print-architecture").output() else {
    return Vec::new();
  };

  let text = String::from_utf8_lossy(&output.stdout);
  let mut out = Vec::new();
  for line in text.lines() {
    let mut parts = line.split_whitespace();
    let _ = parts.next();
    if let Some(arch) = parts.next() {
      out.push(arch.to_string());
    }
  }
  out
}

pub(crate) fn detect_luci_variant() -> String {
  if opkg_is_installed("luci") {
    return "lua1".to_string();
  }
  if opkg_is_installed("luci2") {
    return "js2".to_string();
  }
  "unknown".to_string()
}

pub(crate) fn opkg_is_installed(name: &str) -> bool {
  system::opkg_is_installed(name)
}

pub(crate) fn pkg_list_diff_added(before: &[String], after: &[String]) -> Vec<String> {
  let before_set: std::collections::HashSet<&str> = before.iter().map(String::as_str).collect();
  let mut out = Vec::new();
  for item in after {
    if !before_set.contains(item.as_str()) {
      out.push(item.clone());
    }
  }
  out
}

pub(crate) fn ipk_dir_has_pkg(dir: &Path, pkg: &str) -> bool {
  let Ok(entries) = fs::read_dir(dir) else {
    return false;
  };

  for entry in entries.flatten() {
    let path = entry.path();
    if !path.is_file() {
      continue;
    }
    let name = path.file_name().and_then(|v| v.to_str()).unwrap_or("").to_string();
    if name.starts_with(&format!("{pkg}_")) || name.starts_with(&format!("{pkg}-")) {
      return true;
    }
  }

  false
}

pub(crate) fn precheck_dep_source(dep: &str, ipk_dir: &Path) -> String {
  if opkg_is_installed(dep) {
    return "installed".to_string();
  }
  if opkg_has_package(dep) {
    return "feed".to_string();
  }
  if ipk_dir_has_pkg(ipk_dir, dep) {
    return "packaged".to_string();
  }
  "missing".to_string()
}

pub(crate) fn build_precheck_report(
  manifest: &Manifest,
  depend_dir: &Path,
  binary_dir: &Path,
  app_list: &[String],
) -> Value {
  let kmod = manifest
    .get_array("kmod")
    .into_iter()
    .filter(|dep| !dep.is_empty())
    .map(|dep| json!({"name": dep.clone(), "status": precheck_dep_source(&dep, Path::new(""))}))
    .collect::<Vec<Value>>();

  let base = manifest
    .get_array("base")
    .into_iter()
    .filter(|dep| !dep.is_empty())
    .map(|dep| json!({"name": dep.clone(), "status": precheck_dep_source(&dep, depend_dir)}))
    .collect::<Vec<Value>>();

  let app = app_list
    .iter()
    .filter(|dep| !dep.is_empty())
    .map(|dep| json!({"name": dep.clone(), "status": precheck_dep_source(dep, binary_dir)}))
    .collect::<Vec<Value>>();

  json!({
      "kmod": kmod,
      "base": base,
      "app": app,
  })
}

pub(crate) fn resolve_lang(conf_file: &Path) -> String {
  let conf = read_json_file(conf_file).ok();
  conf
    .and_then(|value| value.get("i18n").and_then(Value::as_str).map(ToString::to_string))
    .unwrap_or_else(|| "zh-cn".to_string())
}

pub(crate) fn reorder_app_list(list: Vec<String>) -> Vec<String> {
  let mut core = Vec::new();
  let mut luci = Vec::new();
  let mut i18n = Vec::new();

  for item in list {
    if item.is_empty() {
      continue;
    }
    if item.starts_with("luci-i18n-") {
      i18n.push(item);
    } else if item.starts_with("luci-app-") {
      luci.push(item);
    } else {
      core.push(item);
    }
  }

  core.extend(luci);
  core.extend(i18n);
  core
}

pub(crate) fn should_use_feeds(
  app: &App,
  label: &str,
  pkg_list: &[String],
  have_ipk_dir: bool,
) -> i8 {
  match app.options.feed_policy {
    FeedPolicy::FeedOnly => {
      if pkg_list.is_empty() {
        -1
      } else {
        1
      }
    }
    FeedPolicy::PackagedOnly => 0,
    FeedPolicy::FeedFirst => {
      if pkg_list.is_empty() {
        0
      } else {
        1
      }
    }
    FeedPolicy::PackagedFirst => {
      if have_ipk_dir {
        0
      } else {
        1
      }
    }
    FeedPolicy::Ask => {
      if !have_ipk_dir {
        return 1;
      }
      if pkg_list.is_empty() {
        return 0;
      }

      opkg_update_ignore();
      let mut total = 0usize;
      let mut available = 0usize;
      for dep in pkg_list {
        if dep.is_empty() {
          continue;
        }
        total += 1;
        if opkg_has_package(dep) {
          available += 1;
        }
      }

      if available == 0 {
        return 0;
      }

      if app.options.non_interactive {
        return 1;
      }

      if prompt_yn(
        &format!(
          "Found {available}/{total} {label} packages in opkg feeds. Use feeds instead of packaged ipks?"
        ),
        true,
      ) {
        1
      } else {
        0
      }
    }
  }
}

pub(crate) fn install_via_feeds_or_ipk(
  app: &App,
  label: &str,
  pkg_list: &[String],
  ipk_dir: &Path,
  have_ipk_dir: bool,
) -> CliResult<()> {
  if pkg_list.is_empty() && !have_ipk_dir {
    return Ok(());
  }

  let use_feeds = should_use_feeds(app, label, pkg_list, have_ipk_dir);
  if use_feeds == -1 {
    return Err(CliError::new(
      E_POLICY_INVALID,
      format!("{label} requires feed packages under feed-only policy"),
    ));
  }

  if use_feeds == 1 {
    if pkg_list.is_empty() {
      return Err(CliError::new(
        E_CONFIG_INVALID,
        format!("{label} packages not defined in manifest"),
      ));
    }
    for dep in pkg_list {
      if dep.is_empty() {
        continue;
      }
      opkg_install_pkg(label, dep)?;
    }
    return Ok(());
  }

  if have_ipk_dir {
    install_ipk_dir(label, ipk_dir)?;
    return Ok(());
  }

  Err(CliError::new(
    E_RUNTIME,
    format!("{label} not available in feeds and no packaged ipks provided"),
  ))
}

pub(crate) fn install_ipk_dir(label: &str, dir: &Path) -> CliResult<()> {
  if !dir.is_dir() {
    return Ok(());
  }

  let mut installed_any = false;
  let entries = fs::read_dir(dir)
    .map_err(|err| CliError::new(E_IO, format!("read {} failed: {err}", dir.display())))?;

  for entry in entries.flatten() {
    let path = entry.path();
    if path.extension().and_then(|v| v.to_str()) != Some("ipk") {
      continue;
    }
    opkg_install_pkg(label, &path.display().to_string())?;
    installed_any = true;
  }

  if !installed_any {
    return Err(CliError::new(
      E_CONFIG_INVALID,
      format!("no ipk files found in {label} dir: {}", dir.display()),
    ));
  }

  Ok(())
}

pub(crate) fn prompt_yn(question: &str, default_yes: bool) -> bool {
  use std::io::{self, BufRead, Write};

  let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
  eprint!("{question} {hint} ");
  let _ = io::stderr().flush();

  let mut input = String::new();
  let stdin = io::stdin();
  let mut handle = stdin.lock();
  if handle.read_line(&mut input).is_err() {
    return default_yes;
  }

  let answer = input.trim().to_ascii_lowercase();
  match answer.as_str() {
    "y" | "yes" => true,
    "n" | "no" => false,
    _ => default_yes,
  }
}

pub(crate) fn run_hook(app: &mut App, tmpdir: &Path, hook_rel: &str, label: &str) -> CliResult<()> {
  if hook_rel.is_empty() {
    return Ok(());
  }

  let hook_path = tmpdir.join(hook_rel);
  if !hook_path.is_file() {
    return Err(CliError::new(E_CONFIG_INVALID, format!("{label} hook not found: {hook_rel}")));
  }

  app.log_info(format!("Running {label} hook: {hook_rel}"));
  run_command_capture_output("sh", &[hook_path.to_string_lossy().as_ref()]).map(|_| ())
}

pub(crate) fn conf_get_i18n(conf_file: &Path) -> Option<String> {
  let conf = read_json_file(conf_file).ok()?;
  let value = conf.get("i18n").and_then(Value::as_str).unwrap_or("zh-cn");
  Some(value.to_string())
}

pub(crate) fn detect_platform() -> String {
  let uname = run_command_text("uname", &["-m"]).unwrap_or_else(|_| "unknown".to_string());
  normalize_uname(&uname)
}

pub(crate) fn detect_os() -> String {
  if let Ok(openwrt_release) = fs::read_to_string("/etc/openwrt_release") {
    if openwrt_release.contains("DISTRIB_ID='OpenWrt'")
      || openwrt_release.contains("DISTRIB_ID=\"OpenWrt\"")
      || openwrt_release.contains("DISTRIB_ID=OpenWrt")
    {
      return "openwrt".to_string();
    }
  }

  "linux".to_string()
}

pub(crate) fn write_file_atomic(path: &Path, content: &[u8]) -> CliResult<()> {
  let mut tmp_name = OsString::from(path.as_os_str());
  tmp_name.push(".tmp");
  let tmp_path = PathBuf::from(tmp_name);

  if let Some(parent) = path.parent() {
    ensure_dir(parent)?;
  }

  fs::write(&tmp_path, content)
    .map_err(|err| CliError::new(E_IO, format!("write {} failed: {err}", tmp_path.display())))?;
  fs::rename(&tmp_path, path)
    .map_err(|err| CliError::new(E_IO, format!("rename {} failed: {err}", path.display())))?;

  Ok(())
}

pub(crate) fn ensure_dir(path: &Path) -> CliResult<()> {
  fs::create_dir_all(path)
    .map_err(|err| CliError::new(E_IO, format!("mkdir {} failed: {err}", path.display())))
}

pub(crate) fn cleanup_pkg_cache_with_notice(app: &mut App) {
  app.log_info("Cleaning up Pica cache");

  let cache_pkgs_dir = app.paths.cache_dir.join("pkgs");
  if !cache_pkgs_dir.is_dir() {
    return;
  }

  let entries = match fs::read_dir(&cache_pkgs_dir) {
    Ok(entries) => entries,
    Err(err) => {
      app.log_warn(format!(
        "cleanup cache skipped: cannot read {}: {err}",
        cache_pkgs_dir.display()
      ));
      return;
    }
  };

  for entry in entries.flatten() {
    let path = entry.path();
    let result = if path.is_dir() { fs::remove_dir_all(&path) } else { fs::remove_file(&path) };

    if let Err(err) = result {
      app.log_warn(format!("cleanup cache failed: {}: {err}", path.display()));
    }
  }
}

pub(crate) fn make_temp_dir(prefix: &str) -> CliResult<PathBuf> {
  core_make_temp_dir(prefix).map_err(CliError::from)
}

pub(crate) fn need_cmd(name: &str) -> CliResult<()> {
  system::need_cmd(name)
}

pub(crate) fn has_command(name: &str) -> bool {
  system::has_command(name)
}

pub(crate) fn opkg_update_ignore() {
  system::opkg_update_ignore();
}

pub(crate) fn opkg_has_package(name: &str) -> bool {
  system::opkg_has_package(name)
}

pub(crate) fn opkg_install_pkg(label: &str, target: &str) -> CliResult<()> {
  system::opkg_install_pkg(label, target)
}

pub(crate) fn run_command_text(program: &str, args: &[&str]) -> CliResult<String> {
  system::run_command_text(program, args)
}

pub(crate) fn run_command_capture_output(program: &str, args: &[&str]) -> CliResult<Vec<u8>> {
  system::run_command_capture_output(program, args)
}

pub(crate) fn copy_dir_recursive(source: &Path, target: &Path) -> CliResult<()> {
  core_copy_dir_recursive(source, target).map_err(CliError::from)
}

pub(crate) fn manifest_get_array(value: &Value, key: &str) -> Vec<String> {
  let Some(entry) = value.get(key) else {
    return Vec::new();
  };

  match entry {
    Value::Array(items) => {
      items.iter().filter_map(Value::as_str).map(ToString::to_string).collect()
    }
    Value::String(text) => vec![text.clone()],
    _ => Vec::new(),
  }
}

pub(crate) fn manifest_get_scalar(value: &Value, key: &str) -> String {
  value.get(key).and_then(Value::as_str).map(ToString::to_string).unwrap_or_default()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn normalize_uname_maps_common_values() {
    assert_eq!(normalize_uname("x86_64"), "amd64");
    assert_eq!(normalize_uname("aarch64"), "arm64");
    assert_eq!(normalize_uname("mips"), "mips");
  }

  #[test]
  fn reorder_app_list_moves_luci_and_i18n_last() {
    let input = vec![
      "luci-i18n-foo-zh-cn".to_string(),
      "foo-core".to_string(),
      "luci-app-foo".to_string(),
      "foo-helper".to_string(),
    ];

    let output = reorder_app_list(input);
    assert_eq!(
      output,
      vec![
        "foo-core".to_string(),
        "foo-helper".to_string(),
        "luci-app-foo".to_string(),
        "luci-i18n-foo-zh-cn".to_string(),
      ]
    );
  }

  #[test]
  fn pkg_list_diff_added_returns_only_new_items() {
    let before = vec!["a".to_string(), "b".to_string()];
    let after = vec!["a".to_string(), "b".to_string(), "c".to_string()];

    let added = pkg_list_diff_added(&before, &after);
    assert_eq!(added, vec!["c".to_string()]);
  }

  #[test]
  fn manifest_helpers_support_scalar_and_array() {
    let value = json!({
        "app": ["foo", "bar"],
        "single": "baz",
        "num": 1
    });

    assert_eq!(manifest_get_array(&value, "app"), vec!["foo", "bar"]);
    assert_eq!(manifest_get_array(&value, "single"), vec!["baz"]);
    assert!(manifest_get_array(&value, "num").is_empty());

    assert_eq!(manifest_get_scalar(&value, "single"), "baz");
    assert_eq!(manifest_get_scalar(&value, "app"), "");
    assert_eq!(manifest_get_scalar(&value, "missing"), "");
  }

  #[test]
  fn should_use_feeds_policy_matrix_basics() {
    let paths = crate::Paths::from_env();
    let base = App::new(
      paths,
      crate::Options {
        json_mode: crate::JsonMode::None,
        non_interactive: true,
        feed_policy: FeedPolicy::FeedOnly,
        fetch_timeout: 30,
        fetch_retry: 2,
        fetch_retry_delay: 1,
      },
    );

    let decision = should_use_feeds(&base, "app", &["a".to_string()], true);
    assert_eq!(decision, 1);

    let no_feed = should_use_feeds(&base, "app", &[], true);
    assert_eq!(no_feed, -1);

    let mut packaged_only = base;
    packaged_only.options.feed_policy = FeedPolicy::PackagedOnly;
    let packaged = should_use_feeds(&packaged_only, "app", &["a".to_string()], true);
    assert_eq!(packaged, 0);

    let mut feed_first = packaged_only;
    feed_first.options.feed_policy = FeedPolicy::FeedFirst;
    let feed_first_choice = should_use_feeds(&feed_first, "app", &["a".to_string()], true);
    assert_eq!(feed_first_choice, 1);

    let mut packaged_first = feed_first;
    packaged_first.options.feed_policy = FeedPolicy::PackagedFirst;
    let packaged_first_choice = should_use_feeds(&packaged_first, "app", &["a".to_string()], true);
    assert_eq!(packaged_first_choice, 0);
  }

  #[test]
  fn install_via_feeds_or_ipk_skips_empty_optional_group() {
    let app = App::new(
      crate::Paths::from_env(),
      crate::Options {
        json_mode: crate::JsonMode::None,
        non_interactive: true,
        feed_policy: FeedPolicy::Ask,
        fetch_timeout: 30,
        fetch_retry: 2,
        fetch_retry_delay: 1,
      },
    );

    let result = install_via_feeds_or_ipk(&app, "base", &[], Path::new("/nonexistent"), false);
    assert!(result.is_ok());
  }
}
