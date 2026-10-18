//! pica-side checks of the version bounds declared in `app` / `base` / `kmod`.
//!
//! A bound is an assertion pica can verify against the installed system, never
//! solver input, so it never reaches a backend command line: `apk` reads a
//! versioned atom, `opkg` installs by name or by `.ipk` path, and handing the
//! same string to both makes one succeed and the other fail.

use crate::app::App;
use pica_pkg_core::constraint::Constraint;

/// The manifest keys that declare dependencies, in the order they are checked.
///
/// `kmod` comes first because it is the one group that pica resolves against a
/// feed regardless of the package's own backend, then the base and app groups.
pub(crate) const GROUPS: [&str; 3] = ["kmod", "base", "app"];

/// The bounds in `constraints` that the installed system does not satisfy.
///
/// Each message names the package that declared the bound, the bound as it was
/// written, and the version actually found: `demo requires base:libfoo>=1.2,
/// but 1.1 is installed`.
///
/// Only an *installed* package can violate a bound. A package the backend has
/// not installed yet is not reported: the backend may still bring the right
/// version, so install defers that case and re-checks once it is done (see
/// `install::pipeline::recheck_constraints`). A name with no version to read is
/// skipped for the same reason — "installed" already tells the caller what it
/// needs.
pub(crate) fn unmet(
  app: &App,
  requester: &str,
  group: &str,
  constraints: &[Constraint],
) -> Vec<String> {
  if constraints.iter().all(|constraint| !constraint.has_bound()) {
    return Vec::new();
  }
  let Ok(package_manager) = app.package_manager() else {
    return Vec::new();
  };

  let mut messages = Vec::new();
  for constraint in constraints {
    if !constraint.has_bound() || !package_manager.is_installed(&constraint.name) {
      continue;
    }
    let Some(installed) = package_manager.installed_version(&constraint.name) else {
      continue;
    };
    if !constraint.is_met_by(&installed) {
      messages.push(format!(
        "{requester} requires {group}:{}, but {installed} is installed",
        constraint.raw
      ));
    }
  }
  messages
}
