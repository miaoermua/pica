use crate::app::{
  App, CliError, CliResult, E_ARG_INVALID, E_CONFIG_INVALID, E_DB_INVALID, ensure_dirs,
};
use crate::candidate::{find_pica_candidates_in_index, not_found_hint};
use crate::i18n::Key;
use crate::installed;
use crate::source;
use crate::state::read_json_file;
use pica_pkg_core::constraint::{self, Constraint};
use pica_pkg_core::manifest::{get_array_for, get_first as manifest_get_first};
use pica_pkg_core::version::{pkgver_cmp_key, pkgver_ge};
use serde_json::Value;

fn installed_entry<'a>(app: &App, db: &'a Value, pkgname: &str) -> CliResult<&'a Value> {
  db.get("installed")
    .and_then(Value::as_object)
    .and_then(|installed| installed.get(pkgname))
    .ok_or_else(|| CliError::new(E_ARG_INVALID, app.args(Key::NotInstalled, &[pkgname])))
}

fn entry_manifest(entry: &Value) -> &Value {
  entry.get("manifest").unwrap_or(&Value::Null)
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
  let db = read_json_file(&app.paths.db_file)?;
  let installed = db
    .get("installed")
    .and_then(Value::as_object)
    .ok_or_else(|| CliError::new(E_DB_INVALID, "db installed is not object"))?;

  let mut names: Vec<String> = installed.keys().cloned().collect();
  names.sort();

  for pkgname in names {
    let Some(entry) = installed.get(&pkgname) else {
      continue;
    };
    let manifest = entry_manifest(entry);
    let pkgver = manifest_get_first(manifest, "pkgver");
    let pkgrel = manifest_get_first(manifest, "pkgrel");
    let version = pkgver_cmp_key(&pkgver, &pkgrel);
    let platform = manifest_get_first(manifest, "platform");

    println!("{pkgname}\t{version}\t{platform}");
  }

  Ok(())
}

pub fn sync_info(app: &mut App, selector: &str) -> CliResult<()> {
  ensure_dirs(&app.paths)?;

  let candidates = find_pica_candidates_in_index(app, selector)?;
  if candidates.is_empty() {
    let hint = not_found_hint(app);
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
  let db = read_json_file(&app.paths.db_file)?;
  let entry = installed_entry(app, &db, pkgname)?;
  let manifest = entry_manifest(entry);

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
  field(
    "Installed At",
    &entry
      .get("installed_at")
      .and_then(Value::as_u64)
      .map(|value| value.to_string())
      .unwrap_or_default(),
  );
  field("Installed From", entry.get("pkgfile").and_then(Value::as_str).unwrap_or(""));
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
fn backend_dependencies(app: &mut App, entry: &Value) -> Option<String> {
  let manifest = entry_manifest(entry);
  let pkgname = manifest_get_first(manifest, "pkgname");
  if pkgname.is_empty() {
    return None;
  }
  let kind = installed::entry_backend(entry).kind;
  if !kind.requires_system_binary() {
    return None;
  }
  app.select_package_manager(kind.as_str()).ok()?;

  let depends = app.package_manager().ok()?.depends(&pkgname);
  (!depends.is_empty()).then(|| depends.join(" "))
}

pub fn files(app: &mut App, pkgname: &str) -> CliResult<()> {
  ensure_dirs(&app.paths)?;
  let db = read_json_file(&app.paths.db_file)?;
  let entry = installed_entry(app, &db, pkgname)?;

  for item in entry.get("files").and_then(Value::as_array).into_iter().flatten() {
    if let Some(path) = item.as_str() {
      println!("{path}");
    }
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
