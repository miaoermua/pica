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

/// What an installed package failed, in the one wording the warning and the
/// selector share.
///
/// It says the assertion did not hold and names no version: the bound is judged
/// from a version pica read once, and the caller adds the declaration it was
/// written as.
pub(crate) const BOUND_UNMET: &str =
  "the installed version does not satisfy the declared constraint";

/// The bounds one declaration fails, or `None` when there is nothing to judge.
///
/// `Some(Vec::new())` is a verdict: the installed version was read and the bound
/// holds. `None` is the absence of one — no bound was declared, nothing is
/// installed, or the backend cannot report the version. The caller must keep the
/// two apart: a bound pica never checked is not a bound that passed, and a
/// backend that answers "installed" without a version cannot be said to satisfy
/// anything.
pub(crate) fn verdict(
  app: &App,
  requester: &str,
  group: &str,
  constraint: &Constraint,
) -> Option<Vec<String>> {
  if !constraint.has_bound() {
    return None;
  }
  let package_manager = app.package_manager().ok()?;
  if !package_manager.is_installed(&constraint.name) {
    return None;
  }
  let installed = package_manager.installed_version(&constraint.name)?;
  if constraint.is_met_by(&installed) {
    return Some(Vec::new());
  }
  Some(vec![format!(
    "{requester} requires {group}:{}, but {installed} is installed",
    constraint.raw
  )])
}

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
  constraints
    .iter()
    .filter_map(|constraint| verdict(app, requester, group, constraint))
    .flatten()
    .collect()
}
