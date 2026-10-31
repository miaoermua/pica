use crate::app::{
  App, CliError, CliResult, E_ARG_INVALID, E_CONFIG_INVALID, JsonMode, ensure_dirs,
};
use crate::candidate::{find_pica_candidates_in_index, not_found_hint};
use crate::commands::search::emit_json;
use crate::i18n::Key;
use crate::installed;
use crate::source;
use crate::state::read_db;
use pica_pkg_core::constraint::{self, Constraint};
use pica_pkg_core::manifest::{get_array_for, get_first as manifest_get_first};
use pica_pkg_core::model::db::{Db, DbEntry};
use pica_pkg_core::version::{pkgver_cmp_key, pkgver_ge};
use serde_json::{Value, json};

fn installed_entry<'a>(app: &App, db: &'a Db, pkgname: &str) -> CliResult<&'a DbEntry> {
  db.installed
    .get(pkgname)
    .ok_or_else(|| CliError::new(E_ARG_INVALID, app.args(Key::NotInstalled, &[pkgname])))
}

#[allow(clippy::cast_precision_loss)]
fn format_size_display(size_text: &str) -> String {
  const KIB: f64 = 1024.0;
  const MIB: f64 = 1024.0 * 1024.0;
  const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

  if size_text.is_empty() {
    return String::new();
  }

  let Ok(size_bytes) = size_text.parse::<u64>() else {
    return size_text.to_string();
  };

  let human = if (size_bytes as f64) >= GIB {
    format!("{:.2} GiB", (size_bytes as f64) / GIB)
  } else if (size_bytes as f64) >= MIB {
    format!("{:.2} MiB", (size_bytes as f64) / MIB)
  } else if (size_bytes as f64) >= KIB {
    format!("{:.2} KiB", (size_bytes as f64) / KIB)
  } else {
    format!("{size_bytes} B")
  };

  format!("{size_bytes} ({human})")
}

pub fn installed(app: &mut App) -> CliResult<()> {
  ensure_dirs(&app.paths)?;
  let db = read_db(&app.paths)?;
  if json_wanted(app) {
    let rows: Vec<Value> = db
      .installed
      .iter()
      .map(|(pkgname, entry)| {
        json!({ "pkgname": pkgname, "version": entry.version(), "platform": entry.field("platform") })
      })
      .collect();
    emit_json(app, &json!({ "packages": rows }));
    return Ok(());
  }
  for (pkgname, entry) in &db.installed {
    println!("{pkgname}\t{}\t{}", entry.version(), entry.field("platform"));
  }

  Ok(())
}

/// `pica -Qo <path>`: which install wrote `path`.
///
/// Pica's own records are asked first, then the system backend, which owns the
/// files of the packages it installed.
///
/// # Errors
/// Fails when nothing claims the path.
pub fn owner(app: &mut App, path: &str) -> CliResult<()> {
  ensure_dirs(&app.paths)?;
  let db = read_db(&app.paths)?;
  let owner =
    db.owner_of(path, "").map(|pkgname| (pkgname.to_string(), "pica".to_string())).or_else(|| {
      let manager = app.package_manager().ok()?;
      manager.owns_file(path).map(|name| (name, manager.kind().as_str().to_string()))
    });
  let Some((pkgname, backend)) = owner else {
    return Err(CliError::new(E_ARG_INVALID, app.args(Key::QueryNoOwner, &[path])));
  };

  if json_wanted(app) {
    emit_json(app, &json!({ "path": path, "pkgname": pkgname, "backend": backend }));
  } else {
    println!("{}", app.args(Key::QueryOwnedBy, &[path, &pkgname, &backend]));
  }
  Ok(())
}

/// `pica -Qu`: installed packages a synced source offers a newer version of.
///
/// # Errors
/// Fails when the database or the sync cache cannot be read.
pub fn upgradable(app: &mut App) -> CliResult<()> {
  ensure_dirs(&app.paths)?;
  let mut rows = Vec::new();
  for record in crate::commands::upgrade::tracked(app)? {
    let Some(newest) = crate::commands::upgrade::newest_offered(app, &record)? else {
      continue;
    };
    if !pkgver_ge(&record.installed, &newest) {
      rows.push((record.pkgname, record.installed, newest));
    }
  }

  if json_wanted(app) {
    let rows: Vec<Value> = rows
      .iter()
      .map(|(pkgname, installed, newest)| {
        json!({ "pkgname": pkgname, "installed": installed, "available": newest })
      })
      .collect();
    emit_json(app, &json!({ "packages": rows }));
    return Ok(());
  }
  for (pkgname, installed, newest) in &rows {
    println!("{pkgname} {installed} -> {newest}");
  }
  Ok(())
}

fn json_wanted(app: &App) -> bool {
  app.options.json_mode == JsonMode::All
}

pub fn sync_info(app: &mut App, selector: &str) -> CliResult<()> {
  ensure_dirs(&app.paths)?;

  let candidates = find_pica_candidates_in_index(app, selector)?;
  if candidates.is_empty() {
    let hint = not_found_hint(app, selector);
    return Err(CliError::new(E_CONFIG_INVALID, app.args(Key::NotFound, &[selector, &hint])));
  }

  let mut best = &candidates[0];
  for candidate in &candidates[1..] {
    if pkgver_ge(&candidate.cmpver, &best.cmpver) {
      best = candidate;
    }
  }

  // `-Si` shows the address the install would actually use, so a relative
  // `download_url` is resolved here the same way.
  let download_url = source::artifact_url(&best.url, best.download_url.as_deref(), &best.filename)
    .unwrap_or_else(|_| best.download_url.clone().unwrap_or_default());

  if json_wanted(app) {
    emit_json(
      app,
      &json!({
        "repo": best.repo, "pkgname": best.pkgname, "appname": best.appname,
        "version": best.cmpver, "pkgver": best.pkgver, "pkgrel": best.pkgrel,
        "branch": best.branch, "protocol": best.protocol, "url": best.pkg_url,
        "luci_url": best.luci_url, "luci_desc": best.luci_desc, "pkgdesc": best.pkgdesc,
        "os": best.os, "platform": best.platform, "arch": best.arch, "pkgmgr": best.pkgmgr,
        "pica": best.min_pica, "download_url": download_url, "filename": best.filename,
        "sha256": best.sha256, "size": best.size, "csize": best.csize,
      }),
    );
    return Ok(());
  }

  println!("Repository      : {}", best.repo);
  println!("Name            : {}", best.pkgname);
  println!("AppName         : {}", best.appname);
  println!("Version         : {}", best.cmpver);
  println!("Pkgver          : {}", best.pkgver);
  println!("Pkgrel          : {}", best.pkgrel);
  println!("Branch          : {}", best.branch);
  println!("Protocol        : {}", best.protocol);
  println!("Program URL     : {}", best.pkg_url);
  println!("LuCI URL        : {}", best.luci_url);
  println!("LuCI Desc       : {}", best.luci_desc);
  println!("PkgDesc         : {}", best.pkgdesc);
  println!("OS              : {}", best.os);
  println!("Platform        : {}", best.platform);
  println!("Arch            : {}", best.arch);
  println!("PkgMgr          : {}", best.pkgmgr);
  println!("Pica Required   : {}", best.min_pica.as_deref().unwrap_or(""));
  println!("Download URL    : {download_url}");
  println!("Filename        : {}", best.filename);
  println!("Sha256          : {}", best.sha256);
  println!(
    "Size            : {}",
    best.size.map(|value| format_size_display(&value.to_string())).unwrap_or_default()
  );

  Ok(())
}

/// Prints one field of the `-Qi` listing, and nothing at all when the row has
/// no value for it.
///
/// A label with nothing after it tells a reader no more than an absent label,
/// and reads as a value that was expected and lost. `Depends` has always been
/// reported this way, because most rows have none; the same holds for the
/// manifest's optional fields, which a row written by any release may or may
/// not carry. The label is padded rather than the line spelled out per field,
/// so a field that is left out does not shift the alignment of the ones below.
fn field(label: &str, value: &str) {
  if !value.is_empty() {
    println!("{label:<16}: {value}");
  }
}

pub fn info(app: &mut App, pkgname: &str) -> CliResult<()> {
  ensure_dirs(&app.paths)?;
  let db = read_db(&app.paths)?;
  let entry = installed_entry(app, &db, pkgname)?;
  let manifest = &entry.manifest;

  if json_wanted(app) {
    let depends = backend_dependencies(app, entry);
    emit_json(
      app,
      &json!({
        "pkgname": pkgname,
        "version": entry.version(),
        "source": installed::entry_source(entry).as_str(),
        "pkgmgr": installed::entry_backend(entry).kind.as_str(),
        "installed_at": entry.installed_at,
        "pkgfile": entry.pkgfile,
        "requires": declared_constraints(manifest, installed::entry_backend(entry).kind.as_str()),
        "depends": depends,
        "manifest": manifest,
      }),
    );
    return Ok(());
  }

  // What the row is, as opposed to what it says about itself: always there.
  println!("Name            : {pkgname}");
  field(
    "Version",
    &pkgver_cmp_key(
      &manifest_get_first(manifest, "pkgver"),
      &manifest_get_first(manifest, "pkgrel"),
    ),
  );
  field("Pkgver", &manifest_get_first(manifest, "pkgver"));
  field("Pkgrel", &manifest_get_first(manifest, "pkgrel"));
  field("AppName", &manifest_get_first(manifest, "appname"));
  let program_url = {
    let value = manifest_get_first(manifest, "url");
    if value.is_empty() { manifest_get_first(manifest, "origin") } else { value }
  };
  field("Program URL", &program_url);
  field("PkgDesc", &manifest_get_first(manifest, "pkgdesc"));
  field("LuCI URL", &manifest_get_first(manifest, "luci_url"));
  field("Protocol", &manifest_get_first(manifest, "protocol"));
  field("Branch", &manifest_get_first(manifest, "branch"));
  field("LuCI Desc", &manifest_get_first(manifest, "luci_desc"));
  field("OS", &manifest_get_first(manifest, "os"));
  field("Platform", &manifest_get_first(manifest, "platform"));
  field("Arch", &manifest_get_first(manifest, "arch"));
  field("Uname", &manifest_get_first(manifest, "uname"));
  field("Type", &manifest_get_first(manifest, "type"));
  // Both fields are shown as resolved rather than raw, so a row written by an
  // older release reports the same channel and backend a fresh install would.
  field("Source", installed::entry_source(entry).as_str());
  field("PkgMgr", installed::entry_backend(entry).kind.as_str());
  field("Packager", &manifest_get_first(manifest, "packager"));
  field("Build Date", &manifest_get_first(manifest, "builddate"));
  field("Size", &format_size_display(&manifest_get_first(manifest, "size")));
  let installed_at =
    if entry.installed_at > 0 { entry.installed_at.to_string() } else { String::new() };
  field("Installed At", &installed_at);
  field("Installed From", &entry.pkgfile);
  field("License", &manifest_get_first(manifest, "license"));
  field("Source Avail", &manifest_get_first(manifest, "source_availability"));
  field("Requires", &declared_constraints(manifest, installed::entry_backend(entry).kind.as_str()));
  if let Some(depends) = backend_dependencies(app, entry) {
    field("Depends", &depends);
  }

  Ok(())
}

/// The bounds the manifest declares, as written, one `group:declaration` each.
///
/// `Depends` reports what the backend resolved; this reports what the artifact
/// asked for, bound included, so a bound is shown as it was written rather than
/// as a comparison pica made up. Names without a bound are left to `Depends`.
///
/// Text that cannot be parsed is treated as no declaration at all: `-Qi` is a
/// read, and refusing to print an installed package over a manifest field would
/// hide more than it reports.
fn declared_constraints(manifest: &Value, backend: &str) -> String {
  let mut declared = Vec::new();
  for group in crate::constraints::GROUPS {
    let items = get_array_for(manifest, group, backend);
    let Ok(constraints) = constraint::parse_all(&items) else {
      continue;
    };
    declared.extend(
      constraints
        .into_iter()
        .filter(Constraint::has_bound)
        .map(|constraint| format!("{group}:{}", constraint.raw)),
    );
  }
  declared.join(" ")
}

/// Dependencies the system backend reports for an entry it installed.
///
/// The backend comes from the entry itself, so rows written before `pkgmgr`
/// existed still resolve. The `pica` backend has no command to ask and declares
/// its dependencies in the manifest instead, so the line is omitted for it. A
/// missing backend command is not an error here: showing information must not
/// fail because the host cannot talk to its package manager.
fn backend_dependencies(app: &mut App, entry: &DbEntry) -> Option<String> {
  let pkgname = entry.field("pkgname");
  if pkgname.is_empty() {
    return None;
  }
  let kind = installed::entry_backend(entry).kind;
  if !kind.requires_system_binary() {
    return None;
  }
  let depends = app
    .with_backend(kind, |app| Ok(app.package_manager().map(|manager| manager.depends(&pkgname))))
    .ok()?
    .ok()?;
  (!depends.is_empty()).then(|| depends.join(" "))
}

pub fn files(app: &mut App, pkgname: &str) -> CliResult<()> {
  ensure_dirs(&app.paths)?;
  let db = read_db(&app.paths)?;
  let entry = installed_entry(app, &db, pkgname)?;
  if json_wanted(app) {
    emit_json(app, &json!({ "pkgname": pkgname, "files": entry.files, "dirs": entry.dirs }));
    return Ok(());
  }
  for path in &entry.files {
    println!("{path}");
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::format_size_display;
  use pretty_assertions::assert_eq;

  #[test]
  fn format_size_display_handles_empty_and_invalid() {
    assert_eq!(format_size_display(""), "");
    assert_eq!(format_size_display("not-a-number"), "not-a-number");
  }

  #[test]
  fn format_size_display_converts_units() {
    assert_eq!(format_size_display("999"), "999 (999 B)");
    assert_eq!(format_size_display("1024"), "1024 (1.00 KiB)");
    assert_eq!(format_size_display("1048576"), "1048576 (1.00 MiB)");
  }
}
