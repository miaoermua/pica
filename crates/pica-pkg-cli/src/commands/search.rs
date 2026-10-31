//! `-Ss`, `-Sl` and `-Qs`: finding packages without installing anything.
//!
//! Everything here reads the sync cache or the database and nothing else, so a
//! search never touches the network or a package manager. Matching ranks and
//! suggests; it never decides what gets installed.

use crate::app::{App, CliError, CliResult, E_ARG_INVALID, E_INDEX_INVALID, JsonMode, ensure_dirs};
use crate::candidate::{NotOffered, offerable};
use crate::i18n::Key;
use crate::platform::detect_os;
use crate::state::{read_db, read_local_index};
use pica_pkg_core::io::now_unix_secs;
use pica_pkg_core::model::db::DbEntry;
use pica_pkg_core::model::local_index::LocalIndex;
use pica_pkg_core::repo::Package;
use pica_pkg_core::search::{Fields, Query, Rank};
use pica_pkg_core::version::pkgver_ge;
use serde_json::{Value, json};
use std::cell::OnceCell;
use std::collections::BTreeMap;

/// An index older than this is mentioned next to the results.
const STALE_AFTER_SECS: u64 = 7 * 24 * 60 * 60;

/// One line of `-Ss` / `-Sl`: the newest entry of one app in one backend of one
/// source, with what this host thinks of it.
struct Hit<'a> {
  rank: Rank,
  repo: &'a str,
  package: &'a Package,
  installed: Option<String>,
  not_offered: Option<NotOffered>,
}

/// `pica -Ss <terms>`: searches the synced sources.
///
/// # Errors
/// Fails when the query is empty or no source has ever been synced.
pub fn sync(app: &mut App, terms: &str, all: bool) -> CliResult<()> {
  let query = Query::new(terms);
  if query.is_empty() {
    return Err(CliError::new(E_ARG_INVALID, app.text(Key::SearchEmptyQuery)));
  }
  let index = synced_index(app)?;
  let hits = collect(app, &index, |fields| query.rank(fields), None)?;
  print_hits(app, &index, &hits, all);
  Ok(())
}

/// `pica -Sl [source]`: lists what the synced sources publish.
///
/// # Errors
/// Fails when no source has been synced, or `source` names one that was not.
pub fn list(app: &mut App, source: Option<&str>, all: bool) -> CliResult<()> {
  let index = synced_index(app)?;
  if let Some(name) = source {
    if !index.repos.contains_key(name) {
      return Err(CliError::new(E_ARG_INVALID, app.args(Key::SearchUnknownSource, &[name])));
    }
  }
  let hits = collect(app, &index, |_| Some(Rank::Exact), source)?;
  print_hits(app, &index, &hits, all);
  Ok(())
}

/// `pica -Qs <terms>`: searches what is installed.
///
/// # Errors
/// Fails when the query is empty or the database cannot be read.
pub fn installed(app: &mut App, terms: &str) -> CliResult<()> {
  let query = Query::new(terms);
  if query.is_empty() {
    return Err(CliError::new(E_ARG_INVALID, app.text(Key::SearchEmptyQuery)));
  }
  ensure_dirs(&app.paths)?;
  let db = read_db(&app.paths)?;

  let mut found = Vec::new();
  for (pkgname, entry) in &db.installed {
    let appname = entry.appname_or(pkgname);
    let pkgdesc = entry.field("pkgdesc");
    let luci_desc = entry.field("luci_desc");
    let descriptions = [pkgdesc.as_str(), luci_desc.as_str()];
    let fields = Fields { appname: &appname, pkgname, descriptions: &descriptions };
    if let Some(rank) = query.rank(&fields) {
      found.push((rank, pkgname.as_str(), entry.version(), pkgdesc));
    }
  }
  found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));

  if app.options.json_mode == JsonMode::All {
    let rows: Vec<Value> = found
      .iter()
      .map(|(rank, name, version, desc)| {
        json!({ "pkgname": name, "version": version, "pkgdesc": desc, "match": rank_name(*rank) })
      })
      .collect();
    emit_json(app, &json!({ "packages": rows }));
    return Ok(());
  }
  for (_, name, version, desc) in &found {
    println!("local/{name} {version}");
    if !desc.is_empty() {
      println!("    {desc}");
    }
  }
  Ok(())
}

/// Names close to `query` among what the synced sources publish, for the hint
/// after a miss. Empty when nothing is close or nothing is synced.
#[must_use]
pub fn suggestions(app: &App, query: &str) -> Vec<String> {
  let Ok(index) = read_local_index(&app.paths) else {
    return Vec::new();
  };
  let mut names: Vec<&str> = index.packages().map(|(_, _, pkg)| pkg.app_key()).collect();
  names.sort_unstable();
  names.dedup();
  pica_pkg_core::search::suggestions(query, names, 5)
}

fn synced_index(app: &App) -> CliResult<LocalIndex> {
  ensure_dirs(&app.paths)?;
  let index = read_local_index(&app.paths)?;
  if index.repos.is_empty() {
    // An empty result here would read as "nothing matches", when the truth is
    // that nothing has been asked yet.
    return Err(CliError::new(E_INDEX_INVALID, app.text(Key::SearchNotSynced)));
  }
  Ok(index)
}

/// Every matching entry, one per source, backend and app: the newest version.
fn collect<'a>(
  app: &App,
  index: &'a LocalIndex,
  rank: impl Fn(&Fields<'_>) -> Option<Rank>,
  only_repo: Option<&str>,
) -> CliResult<Vec<Hit<'a>>> {
  let db = read_db(&app.paths)?;
  let host_os = detect_os();
  let host_arches = OnceCell::new();

  let mut newest: BTreeMap<(&str, &str, &str), Hit<'a>> = BTreeMap::new();
  for (repo, _, package) in index.packages() {
    if only_repo.is_some_and(|only| only != repo) {
      continue;
    }
    let descriptions: Vec<&str> =
      [package.pkgdesc.as_deref(), package.luci_desc.as_deref()].into_iter().flatten().collect();
    let fields =
      Fields { appname: package.app_key(), pkgname: &package.pkgname, descriptions: &descriptions };
    let Some(found) = rank(&fields) else {
      continue;
    };

    let key = (repo, package.pkgmgr.as_str(), package.app_key());
    if newest
      .get(&key)
      .is_some_and(|hit| pkgver_ge(&hit.package.version_key(), &package.version_key()))
    {
      continue;
    }
    let not_offered = offerable(app, &host_arches, &host_os, app.backend(), package).err();
    let installed = db.installed.get(&package.pkgname).map(DbEntry::version);
    newest.insert(key, Hit { rank: found, repo, package, installed, not_offered });
  }

  let mut hits: Vec<Hit<'a>> = newest.into_values().collect();
  hits.sort_by(|a, b| {
    a.rank
      .cmp(&b.rank)
      .then(a.not_offered.is_some().cmp(&b.not_offered.is_some()))
      .then_with(|| a.package.app_key().cmp(b.package.app_key()))
      .then_with(|| a.repo.cmp(b.repo))
  });
  Ok(hits)
}

fn print_hits(app: &mut App, index: &LocalIndex, hits: &[Hit<'_>], all: bool) {
  let hidden = hits.iter().filter(|hit| hit.not_offered.is_some()).count();
  let shown: Vec<&Hit<'_>> = hits.iter().filter(|hit| all || hit.not_offered.is_none()).collect();
  let synced_at = index.oldest_sync();

  if app.options.json_mode == JsonMode::All {
    let rows: Vec<Value> = shown
      .iter()
      .map(|hit| {
        json!({
          "repo": hit.repo,
          "appname": hit.package.app_key(),
          "pkgname": hit.package.pkgname,
          "version": hit.package.version_key(),
          "pkgmgr": hit.package.pkgmgr,
          "pkgdesc": hit.package.pkgdesc,
          "installable": hit.not_offered.is_none(),
          "not_installable_reason": hit.not_offered.map(NotOffered::as_str),
          "installed_version": hit.installed,
          "match": rank_name(hit.rank),
        })
      })
      .collect();
    emit_json(app, &json!({ "packages": rows, "hidden": hidden, "synced_at": synced_at }));
    return;
  }

  for hit in &shown {
    let mut line = format!(
      "{}/{} {} [{}]",
      hit.repo,
      hit.package.app_key(),
      hit.package.version_key(),
      hit.package.pkgmgr
    );
    if hit.package.pkgname != hit.package.app_key() {
      line.push_str(" (");
      line.push_str(&hit.package.pkgname);
      line.push(')');
    }
    if let Some(version) = &hit.installed {
      line.push(' ');
      line.push_str(&app.args(Key::SearchInstalled, &[version]));
    }
    if let Some(reason) = hit.not_offered {
      line.push(' ');
      line.push_str(&app.args(Key::SearchNotInstallable, &[reason.as_str()]));
    }
    println!("{line}");
    if let Some(desc) = hit.package.pkgdesc.as_deref().filter(|desc| !desc.is_empty()) {
      println!("    {desc}");
    }
  }
  if hidden > 0 && !all {
    let count = hidden.to_string();
    app.info_args(Key::SearchHidden, &[&count]);
  }
  if let Some(age) = synced_at.and_then(|at| now_unix_secs().checked_sub(at)) {
    if age > STALE_AFTER_SECS {
      let days = (age / (24 * 60 * 60)).to_string();
      app.info_args(Key::IndexStale, &[&days]);
    }
  }
}

/// Prints one JSON document as the whole output. Marked as text so the generic
/// success object is not printed after it.
pub(crate) fn emit_json(app: &mut App, value: &Value) {
  println!("{value}");
  app.json_has_text = true;
}

fn rank_name(rank: Rank) -> &'static str {
  match rank {
    Rank::Exact => "exact",
    Rank::Prefix => "prefix",
    Rank::Name => "name",
    Rank::Description => "description",
    Rank::Close => "close",
  }
}
