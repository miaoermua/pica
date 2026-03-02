use crate::app::{ensure_dirs, App, CliError, CliResult, E_CONFIG_INVALID, E_INDEX_INVALID};
use crate::platform::detect_os;
use crate::state::read_json_file;
use pica_pkg_core::selector::Selector;
use pica_pkg_core::version::pkgver_cmp_key;
use serde_json::Value;

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
