mod feed;
mod pipeline;
mod precheck;

use crate::app::{
  conf_get_i18n, ensure_dirs, App, CliError, CliResult, E_CONFIG_INVALID, E_INTEGRITY_INVALID,
  E_REPO_INVALID, E_VERSION_INCOMPATIBLE,
};
use crate::candidate::find_pica_candidates_in_index;
use crate::platform::{detect_os, detect_platform};
use crate::state::{db_find_installed_pkgname_by_selector, db_set_installed, read_json_file};
use crate::system::{
  fetch_url, need_cmd, opkg_has_package, opkg_install_pkg, opkg_installed_version,
  opkg_update_ignore,
};
use pica_pkg_core::io::now_unix_secs;
use pica_pkg_core::manifest::get_first as manifest_get_first;
use pica_pkg_core::repo::is_supported_url;
use pica_pkg_core::selector::Selector;
use pica_pkg_core::version::{pkgver_ge, ver_ge};
use pica_pkg_core::PICA_VERSION;
use serde_json::{json, Value};
use std::path::Path;

pub fn app_auto(app: &mut App, selector: &str) -> CliResult<()> {
  ensure_dirs(&app.paths)?;

  let parsed = Selector::parse(selector).map_err(|err| CliError::new(E_CONFIG_INVALID, err))?;
  let installed_pkg = db_find_installed_pkgname_by_selector(&app.paths.db_file, &parsed)?;
  if let Some(pkgname) = installed_pkg {
    let db = read_json_file(&app.paths.db_file)?;
    let source = db
      .get("installed")
      .and_then(Value::as_object)
      .and_then(|installed| installed.get(&pkgname))
      .and_then(|entry| entry.get("manifest"))
      .map(|manifest| manifest_get_first(manifest, "source"))
      .unwrap_or_default();

    match source.as_str() {
      "opkg" => {
        app_via_opkg(app, selector)?;
        return Ok(());
      }
      "pica" => {
        pica_from_repo(app, selector)?;
        return Ok(());
      }
      _ => {}
    }
  }

  opkg_update_ignore();

  if !Selector::is_structured(selector) && opkg_has_package(&parsed.appname) {
    let should_install_opkg = if app.options.non_interactive {
      matches!(
        app.options.feed_policy,
        crate::app::FeedPolicy::Ask
          | crate::app::FeedPolicy::FeedFirst
          | crate::app::FeedPolicy::FeedOnly
      )
    } else {
      !matches!(app.options.feed_policy, crate::app::FeedPolicy::PackagedOnly)
    };

    if should_install_opkg {
      app_via_opkg(app, selector)?;
      return Ok(());
    }
  }

  pica_from_repo(app, selector)
}

pub fn app_via_opkg(app: &mut App, selector: &str) -> CliResult<()> {
  ensure_dirs(&app.paths)?;
  need_cmd("opkg")?;

  let parsed = Selector::parse(selector).map_err(|err| CliError::new(E_CONFIG_INVALID, err))?;
  let appname = parsed.appname.clone();

  opkg_update_ignore();
  let lang = conf_get_i18n(&app.paths.conf_file).unwrap_or_else(|| "zh-cn".to_string());

  let mut candidates = vec![appname.clone(), format!("luci-app-{appname}")];
  if lang == "zh-cn" {
    candidates.push(format!("luci-i18n-{appname}-{lang}"));
  }

  let mut to_install = Vec::new();
  for pkg in candidates {
    if opkg_has_package(&pkg) {
      to_install.push(pkg);
    }
  }

  if to_install.is_empty() {
    return Err(CliError::new(E_CONFIG_INVALID, format!("opkg: package not found: {appname}")));
  }

  app.log_info(format!("Installing (opkg): {}", to_install.join(" ")));

  for pkg in &to_install {
    opkg_install_pkg("opkg", pkg)?;
  }

  let mut base_ver = opkg_installed_version(&appname).unwrap_or_default();
  if base_ver.is_empty() {
    if let Some(first_pkg) = to_install.first() {
      base_ver = opkg_installed_version(first_pkg).unwrap_or_default();
    }
  }

  let manifest = json!({
      "pkgname": appname,
      "appname": appname,
      "branch": parsed.branch,
      "pkgver": base_ver,
      "os": detect_os(),
      "platform": detect_platform(),
      "arch": "all",
      "source": "opkg",
      "pkgmgr": "opkg",
      "opkg": to_install,
  });

  db_set_installed(&app.paths.db_file, &appname, &manifest, "opkg", &to_install)?;
  app.log_info("Transaction completed");
  Ok(())
}

pub fn pica_from_repo(app: &mut App, selector: &str) -> CliResult<()> {
  ensure_dirs(&app.paths)?;
  need_cmd("tar")?;

  let candidates = find_pica_candidates_in_index(app, selector)?;
  if candidates.is_empty() {
    return Err(CliError::new(
      E_CONFIG_INVALID,
      format!("package not found in pica repos: {selector}"),
    ));
  }

  let mut best_index = 0usize;
  let mut best_ver = String::new();
  for (index, candidate) in candidates.iter().enumerate() {
    if pkgver_ge(&candidate.cmpver, &best_ver) {
      best_index = index;
      best_ver.clone_from(&candidate.cmpver);
    }
  }

  let best = &candidates[best_index];

  if let Some(min_pica) = &best.min_pica {
    if !min_pica.is_empty() && !ver_ge(PICA_VERSION, min_pica) {
      return Err(CliError::new(
        E_VERSION_INCOMPATIBLE,
        format!("pica too old: pkg requires >= {min_pica}, cli is {PICA_VERSION}"),
      ));
    }
  }

  if best.filename.contains('/') || !best.filename.ends_with(".pkg.tar.gz") {
    return Err(CliError::new(
      E_REPO_INVALID,
      format!("{}: invalid filename: {}", best.repo, best.filename),
    ));
  }

  if best.sha256.len() != 64 || !best.sha256.chars().all(|ch| ch.is_ascii_hexdigit()) {
    return Err(CliError::new(
      E_REPO_INVALID,
      format!("{}: invalid sha256 for {}", best.repo, best.pkgname),
    ));
  }

  let download_url = if let Some(url) = &best.download_url {
    if !is_supported_url(url) {
      return Err(CliError::new(
        E_REPO_INVALID,
        format!("{}: invalid download_url for {}: {url}", best.repo, best.pkgname),
      ));
    }
    url.clone()
  } else {
    format!("{}/packages/{}", best.url.trim_end_matches('/'), best.filename)
  };

  let cache_pkgs = app.paths.cache_dir.join("pkgs");
  pica_pkg_core::io::ensure_dir(&cache_pkgs).map_err(CliError::from)?;
  let cached = cache_pkgs.join(&best.filename);

  app.log_info(format!("Downloading {} ({}) from {}...", best.pkgname, best.cmpver, best.repo));
  let raw = fetch_url(
    &download_url,
    is_supported_url,
    app.options.fetch_timeout,
    app.options.fetch_retry,
    app.options.fetch_retry_delay,
  )?;

  let actual_sha256 = pica_pkg_core::io::sha256_hex(&raw);
  if !actual_sha256.eq_ignore_ascii_case(&best.sha256) {
    return Err(CliError::new(
      E_INTEGRITY_INVALID,
      format!(
        "sha256 mismatch for {}: expected {}, got {}",
        best.filename, best.sha256, actual_sha256
      ),
    ));
  }

  pica_pkg_core::io::write_atomic(&cached, &raw).map_err(CliError::from)?;

  pipeline::pkgfile(app, &cached, Some(selector.to_string()))
}

pub fn pkg_source(app: &mut App, source: &str, selector: Option<String>) -> CliResult<()> {
  if Path::new(source).is_file() {
    return pipeline::pkgfile(app, Path::new(source), selector);
  }

  if is_supported_url(source) {
    ensure_dirs(&app.paths)?;
    need_cmd("tar")?;

    let cache_pkgs = app.paths.cache_dir.join("pkgs");
    pica_pkg_core::io::ensure_dir(&cache_pkgs).map_err(CliError::from)?;

    let guessed = sanitize_cache_filename(
      Path::new(source).file_name().and_then(|value| value.to_str()).unwrap_or("remote"),
    );
    let cached = cache_pkgs.join(guessed);

    app.log_info(format!("Downloading pkg from URL: {source}"));
    let raw = fetch_url(
      source,
      is_supported_url,
      app.options.fetch_timeout,
      app.options.fetch_retry,
      app.options.fetch_retry_delay,
    )?;
    pica_pkg_core::io::write_atomic(&cached, &raw).map_err(CliError::from)?;
    return pipeline::pkgfile(app, &cached, selector);
  }

  Err(CliError::new(E_CONFIG_INVALID, format!("pkg source not found or unsupported URL: {source}")))
}

pub fn sanitize_cache_filename(value: &str) -> String {
  let mut out = String::new();
  for ch in value.chars() {
    if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '+' | '-') {
      out.push(ch);
    } else {
      out.push('_');
    }
  }

  if out.is_empty() {
    out = format!("remote-{}-{}", now_unix_secs(), std::process::id());
  }
  if !out.ends_with(".pkg.tar.gz") {
    out.push_str(".pkg.tar.gz");
  }

  out
}

#[cfg(test)]
mod tests {
  use super::sanitize_cache_filename;
  use pretty_assertions::assert_eq;

  #[test]
  fn sanitize_cache_filename_keeps_extension() {
    let value = sanitize_cache_filename("hello package");
    assert!(value.ends_with(".pkg.tar.gz"));
    assert!(!value.contains(' '));
  }

  #[test]
  fn sanitize_cache_filename_preserves_valid_chars() {
    let value = sanitize_cache_filename("test-1.0_beta+2.pkg.tar.gz");
    assert_eq!(value, "test-1.0_beta+2.pkg.tar.gz");
  }
}
