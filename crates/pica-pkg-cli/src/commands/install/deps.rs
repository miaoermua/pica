//! Steps 3 and 5: what the artifact depends on, and installing it.

use super::feed::{
  DependencySource, dependency_source, install_pkgs_dir, pkg_list_diff_added, reorder_app_list,
};
use super::optional::{self, Plan as OptionalPlan};
use super::pipeline::{DependencyPrecheck, InstallContext};
use super::precheck::{DeclaredDeps, build_precheck_report, summarize_missing_precheck};
use super::transaction::InstallTransaction;
use crate::app::{App, CliError, CliResult, E_PACKAGE_INVALID, E_RUNTIME, FeedPolicy};
use crate::i18n::Key;
use crate::i18n::tr;
use crate::pkgmgr::PackageManagerKind;
use pica_pkg_core::constraint::{self, Constraint};
use pica_pkg_core::layout::{DEFAULT_LUCI_I18N, PKGS, ROOT};
use serde_json::{Value, json};

/// Which backend installs this artifact's declared dependencies, if any.
///
/// `opkg` and `apk` artifacts answer with themselves. A `pica` artifact answers
/// with this host's manager: pica resolves nothing, but `app`/`base`/`kmod` is
/// still a request to a package database, and the only ones on a router belong
/// to `opkg` or `apk`. Not solving dependencies and having them installed are
/// different questions; only the first is pica's non-goal.
///
/// The declarations themselves are read in the artifact's own backend scope
/// (`ctx.pkgmgr`), so `base@pica` means "only when built for pica" and is never
/// silently dropped because the host happens to run opkg.
///
/// `None` means nothing here can install them: the host has no system package
/// manager, which is the case `pica` exists for. The artifact still installs.
pub(super) fn dependency_backend(app: &App, pkgmgr: &str) -> Option<PackageManagerKind> {
  let kind = PackageManagerKind::parse(pkgmgr)?;
  if kind.requires_system_binary() {
    return Some(kind);
  }
  app.system_backend()
}

/// Whether this install has any dependency work at all.
///
/// A `pica` artifact borrows the package manager for the dependencies it names
/// and for nothing else, so one that declares none must leave the host's
/// database alone: no index refresh, no installed list, no rollback baseline.
pub(super) fn touches_package_manager(pkgmgr: &str, deps: &DeclaredDeps) -> bool {
  pkgmgr != PackageManagerKind::Pica.as_str()
    || !(deps.kmod.is_empty() && deps.base.is_empty() && deps.app.is_empty())
}

pub(super) fn non_empty(items: Vec<String>) -> Vec<String> {
  items.into_iter().filter(|item| !item.is_empty()).collect()
}

/// Brings the host's dependency index up to date, or establishes that this
/// artifact does not need it.
///
/// The precheck below calls a dependency missing when the backend's index does
/// not list it, so an index that cannot be read has to stop the run: a router
/// with unreachable feeds would otherwise be told its dependencies were missing
/// and the install would be refused for them.
///
/// An artifact that requires nothing has nothing the index could judge. Its
/// optional entries are skipped by the missing summary and already degrade to
/// "unknown" on their own, so an unreadable index there costs those options
/// their offer and nothing else — and blocking the install would turn an
/// optional dependency into a mandatory one.
pub(super) fn ensure_index(app: &mut App, deps: &DeclaredDeps) -> CliResult<()> {
  let Err(err) = app.package_manager()?.update_index() else {
    return Ok(());
  };
  if declares_required(deps) {
    return Err(err);
  }
  app.log_warn(tr(Key::OptionalIndexUnavailable, &[&err.message]));
  Ok(())
}

/// Whether any of the three groups declares something required, `kmod`
/// included: the index answers for kernel modules exactly as it does for
/// packages, so a group left out here would unlock the downgrade on an artifact
/// that still needs the answer.
pub(super) fn declares_required(deps: &DeclaredDeps) -> bool {
  [&deps.kmod, &deps.base, &deps.app]
    .into_iter()
    .any(|constraints| constraint::required(constraints).next().is_some())
}

/// Step 3: works out which dependencies this install would need and whether they
/// can be satisfied.
///
/// Read-only: it drives the package manager's index and queries only, so a
/// failure here leaves nothing behind to roll back.
pub(super) fn precheck_dependencies(
  app: &mut App,
  ctx: &InstallContext<'_>,
  deps_from: Option<PackageManagerKind>,
) -> CliResult<DependencyPrecheck> {
  let pkgs_dir = ctx.tmpdir.join(PKGS);

  // Nothing on this host can install a dependency: no `opkg`, no `apk`. The
  // artifact still installs — serving such a host is what `pica` is for — but a
  // declaration that cannot be honoured is named, not passed over.
  if deps_from.is_none() {
    let declared_app = non_empty(ctx.manifest.get_array_for("app", ctx.pkgmgr));
    let deps = DeclaredDeps::parse(ctx.manifest, ctx.pkgmgr, None, &declared_app)?;
    let mut uninstallable = required_names(&deps.kmod);
    uninstallable.extend(required_names(&deps.base));
    uninstallable.extend(required_names(&deps.app));
    if !uninstallable.is_empty() {
      app.log_warn(tr(Key::DependenciesUninstalled, &[&uninstallable.join(" ")]));
    }
    let optional = OptionalPlan::unavailable(&deps, super::optional::PICA_CANNOT_INSTALL);
    optional.warn_skipped(app);
    return Ok(DependencyPrecheck {
      report: json!({"kmod": [], "base": [], "app": []}),
      deps,
      source: None,
      optional,
    });
  }

  let mut app_list = non_empty(ctx.manifest.get_array_for("app", ctx.pkgmgr));
  let lang = crate::state::luci_i18n(&app.paths);
  let i18n_template = ctx.manifest.get_first("app_i18n");
  if !i18n_template.is_empty() && lang == DEFAULT_LUCI_I18N {
    app_list.push(i18n_template.replace("{lang}", &lang));
  }
  app_list = reorder_app_list(app_list);

  // Names what the precheck below actually does: it reads the artifact's
  // declarations, asks the backend's index about each one and reports where it
  // would come from. It is not a solver — no version is chosen, no conflict
  // between packages is detected — so it must not be called one.
  app.log_info(tr(Key::CheckingDeps, &[]));

  let deps = DeclaredDeps::parse(
    ctx.manifest,
    ctx.pkgmgr,
    deps_from.map(PackageManagerKind::as_str),
    &app_list,
  )?;

  if !touches_package_manager(ctx.pkgmgr, &deps) {
    let optional = OptionalPlan::unavailable(&deps, super::optional::PICA_CANNOT_INSTALL);
    return Ok(DependencyPrecheck {
      report: json!({"kmod": [], "base": [], "app": []}),
      deps,
      source: None,
      optional,
    });
  }

  ensure_index(app, &deps)?;

  let report = build_precheck_report(app, ctx.manifest, &pkgs_dir, &deps)?;
  let missing = summarize_missing_precheck(&report);
  if !missing.is_empty() {
    app.log_warn(tr(Key::DepPrecheckMissing, &[&missing.join(" ")]));
    let hint = format!("{}{}", pica_package_hint(app, &missing), other_backend_hint(deps_from));
    return Err(CliError::new(E_RUNTIME, tr(Key::DepPrecheckFailed, &[&missing.join(" "), &hint])));
  }

  // The source is decided before the selector, not in step 5: an option that
  // cannot be installed from the policy's source must not be offered.
  let source = dependency_source_for(app, &deps, pkgs_dir.is_dir())?;
  let optional = optional::plan(app, ctx.pkgname, &deps, &report, source);
  optional.warn_skipped(app);
  if optional.selected_count() > 0 {
    app.log_info(tr(Key::InstallingOptional, &[&optional.selected_count()]));
  }
  for line in dependency_source_lines(&report, &optional) {
    app.log_info(line);
  }

  Ok(DependencyPrecheck { report, deps, source, optional })
}

/// Says, for the missing names that are published as Pica packages, why that
/// does not make them installable.
///
/// A dependency name is resolved against the backend's installed set, the
/// backend's feeds and the artifact's own `pkgs/`. A Pica package is in none of
/// those: pica keeps its own ledger and never reads another artifact's manifest,
/// so "it is in the source" and "it can be a dependency" are different things.
/// Naming that difference is the whole point — otherwise the user reads "missing"
/// and goes looking for the package they just saw listed.
///
/// The synced index is read directly, without the host filters the selector
/// applies: the question here is whether the name is published at all, not
/// whether it would install on this machine. No network request is made. An
/// index that cannot be read is not an answer about the sources, so the hint is
/// omitted rather than claiming the package is unavailable.
pub(super) fn pica_package_hint(app: &App, missing: &[String]) -> String {
  let Ok(index) = crate::state::read_local_index(&app.paths) else {
    return String::new();
  };

  let mut found = Vec::new();
  for entry in missing {
    let name = entry.split_once(':').map_or(entry.as_str(), |(_, name)| name);
    let published =
      index.packages().any(|(_, _, pkg)| pkg.app_key() == name || pkg.pkgname == name);
    if published {
      found.push(name.to_string());
    }
  }
  if found.is_empty() {
    return String::new();
  }
  tr(Key::DepIsPicaPackage, &[&found.join(" ")])
}

/// Names a second system package manager this host has and this install did not
/// search.
///
/// One install has one executor: `App::system_backend` picks `apk` or `opkg` and
/// the precheck asks that database alone. A dependency only the other one
/// publishes therefore comes back "missing" — true of the database that was
/// asked, false of the host. Naming it turns "missing kmod-tun" into something
/// to do: the manager is right there, and `--pkgmgr` moves the install onto it.
///
/// Searching both is the thing this deliberately does not do. Two databases can
/// each carry the package, and pica could not then say which one's files the
/// artifact is running against — nor which one to ask when it is removed.
fn other_backend_hint(deps_from: Option<PackageManagerKind>) -> String {
  let Some(from) = deps_from else {
    return String::new();
  };
  let Some(other) = crate::pkgmgr::available_kinds()
    .into_iter()
    .find(|kind| kind.requires_system_binary() && *kind != from)
  else {
    return String::new();
  };
  let (other, from) = (other.as_str(), from.as_str());
  tr(Key::DepPrecheckOtherBackend, &[&other, &from])
}

/// The container-or-feeds decision, taken once in the precheck.
///
/// [`dependency_source`] answers for the required dependencies and keeps its
/// existing errors and prompts. The one case it does not answer is an artifact
/// whose required lists are empty: it returns `None` there, and an optional
/// dependency still needs a source to be installable. With no `pkgs/` the feeds
/// are the only candidate, and `packaged-only` is the one policy that forbids
/// them — in which case there is nothing to offer and the install proceeds.
pub(super) fn dependency_source_for(
  app: &mut App,
  deps: &DeclaredDeps,
  have_pkgs_dir: bool,
) -> CliResult<Option<DependencySource>> {
  let declared: Vec<String> =
    required_names(&deps.base).into_iter().chain(required_names(&deps.app)).collect();

  if declared.is_empty() && !have_pkgs_dir {
    return Ok(match app.options.feed_policy {
      FeedPolicy::PackagedOnly => None,
      _ => Some(DependencySource::Feeds),
    });
  }

  dependency_source(app, &declared, have_pkgs_dir)
}

/// The declarations a backend must be given: the required ones.
pub(super) fn required_names(constraints: &[Constraint]) -> Vec<String> {
  constraint::required(constraints).map(|constraint| constraint.name.clone()).collect()
}

/// The dependencies this install has to do something about, each with the source
/// the precheck found for it: `kmod-tun (feed)`, `helper (packaged)`.
///
/// Installed ones are left out — nothing is done about them — and the status
/// word is the one the precheck recorded, so the line cannot claim a source the
/// check did not find. An optional dependency is listed only when this install
/// covers it: a declined one is not being brought in, and saying `(feed)` next to
/// it would read as though it were.
pub(super) fn dependency_source_lines(report: &Value, optional: &OptionalPlan) -> Vec<String> {
  let mut lines = Vec::new();

  for group in crate::constraints::GROUPS {
    let Some(entries) = report.get(group).and_then(Value::as_array) else {
      continue;
    };
    for entry in entries {
      let status = entry.get("status").and_then(Value::as_str).unwrap_or("");
      if status != "feed" && status != "packaged" {
        continue;
      }
      let name = entry.get("name").and_then(Value::as_str).unwrap_or("");
      if name.is_empty() {
        continue;
      }
      let is_optional = entry.get("optional").and_then(Value::as_bool).unwrap_or(false);
      if is_optional && !optional.covers(group, name) {
        continue;
      }
      lines.push(format!("  {name} ({status})"));
    }
  }

  lines
}

/// One feed phase: what must be installed plus what the user chose.
///
/// `resolve` already removed any name declared both ways, so the two halves
/// cannot hold the same package and the request holds each name once.
pub(super) fn phase_names(
  constraints: &[Constraint],
  precheck: &DependencyPrecheck,
  group: &str,
) -> Vec<String> {
  let mut names = required_names(constraints);
  names.extend(precheck.optional.selected_names(group));
  names
}

/// Installs one dependency phase, skipping what is already installed.
///
/// The filter is what keeps an install from touching packages it did not come
/// for: a backend asked for a name it already has may upgrade it, and pica has
/// no upgrade policy on this path — that decision belongs to `-Su`. A dependency
/// whose installed version misses a declared bound never reaches here: the
/// precheck refuses it when it is required, and the selector never offers it
/// when it is optional.
///
/// The bookkeeping runs whatever the command returned, because a backend that
/// fails partway has still installed part of the request. See
/// [`InstallTransaction::record_backend_changes`].
pub(super) fn install_phase(
  app: &mut App,
  tx: &mut InstallTransaction,
  label: &str,
  names: &[String],
) -> CliResult<()> {
  if names.is_empty() {
    return Ok(());
  }

  let missing: Vec<String> = {
    let manager = app.package_manager()?;
    names.iter().filter(|name| !manager.is_installed(name)).cloned().collect()
  };
  if missing.is_empty() {
    return Ok(());
  }

  let result = app.package_manager()?.install(label, &missing);
  tx.record_backend_changes(app);
  result
}

/// Step 5: installs whatever the artifact's dependencies need, and nothing when
/// it has none.
///
/// A system backend is asked to install the app/base/kmod lists — from its feeds
/// when the policy says so, from the containers in the artifact otherwise. Which
/// of the two is decided once, before either phase runs: see
/// [`dependency_source`]. The two phases are then either two feed requests, in
/// `base`-then-`app` order, or one request holding the whole container batch.
///
/// The `pica` backend has no feeds and no solver, so it is given no dependency
/// work at all: its artifact ships `root/` and `scripts/`, and `deploy_payload`
/// lays those files down. A `pica` artifact carrying `pkgs/` is refused instead
/// of half-installed: pica does not read container formats, so it could neither
/// install the container nor account for the files inside it.
pub(super) fn install_dependency_containers(
  app: &mut App,
  ctx: &InstallContext<'_>,
  precheck: &DependencyPrecheck,
  tx: &mut InstallTransaction,
  deps_from: Option<PackageManagerKind>,
) -> CliResult<()> {
  let pkgs_dir = ctx.tmpdir.join(PKGS);

  // A `pica` artifact never carries containers, whoever installs its
  // dependencies: pica reads no container format, and the dependencies it
  // declares come from the feeds the host's manager already knows.
  if ctx.pkgmgr == PackageManagerKind::Pica.as_str() && pkgs_dir.is_dir() {
    return Err(CliError::new(
      E_PACKAGE_INVALID,
      tr(Key::PicaArtifactWithPkgs, &[&PKGS, &ROOT, &ROOT]),
    ));
  }

  // Nobody to install them: the precheck already warned, and the artifact's own
  // files are `deploy_payload`'s business.
  if deps_from.is_none() {
    return Ok(());
  }

  // kmods are always resolved through the feeds: an OpenWrt kernel module is not
  // something an application artifact can carry (see `build_precheck_report`).
  // One request for the missing ones, so the backend sees the set at once.
  // A chosen optional kmod joins that one request; an installed one never does,
  // which is the rule every other phase follows too (`install_phase`).
  let mut kmods = required_names(&precheck.deps.kmod);
  kmods.extend(precheck.optional.selected_names("kmod"));
  install_phase(app, tx, "kmod", &kmods)?;

  // The source was decided in the precheck (see `dependency_source_for`), so the
  // selector could only offer what this install can deliver.
  let Some(source) = precheck.source else {
    return Ok(());
  };

  match source {
    // Feeds are installed in phases, base first: a base dependency has to be on
    // the device before the app asks its own resolver for anything. Each phase is
    // one request, so a name in a phase can depend on a sibling of the same phase.
    // Only the names go into the requests: the bounds are pica's to check, and
    // opkg would read `libfoo>=1.2` as a package called that.
    DependencySource::Feeds => {
      install_phase(app, tx, "base", &phase_names(&precheck.deps.base, precheck, "base"))?;
      let before_app = app.package_manager()?.snapshot_installed();
      install_phase(app, tx, "app", &phase_names(&precheck.deps.app, precheck, "app"))?;
      let after_app = app.package_manager()?.snapshot_installed();
      match (before_app, after_app) {
        (Ok(before), Ok(after)) => tx.app_added = pkg_list_diff_added(&before, &after),
        // Which packages the `app` phase added is a report field; no rollback
        // depends on it. An unreadable list costs the report, not the install —
        // and leaving it empty is better than claiming they were all added.
        (before, after) => {
          let err = before.err().or(after.err()).expect("one of the two failed");
          app.log_warn(tr(Key::AppPhaseUnknownAdded, &[&err.message]));
        }
      }
    }
    // The artifact's containers are one batch, installed once. Nothing in the
    // artifact says which container serves `base` and which serves `app`, and the
    // containers of one application depend on each other, so the batch is left
    // whole: a split would be a guess, and a guess that separates two dependent
    // containers fails the install outright.
    DependencySource::Containers => {
      let result = install_pkgs_dir(app, "packaged", &pkgs_dir);
      // A batch that fails partway has still put some of its containers on the
      // device, and the backend may have pulled dependencies in with them.
      tx.record_backend_changes(app);
      result?;
    }
  }

  Ok(())
}

/// Re-checks the artifact's bounds once the backend has installed its part.
///
/// The precheck can only refuse a bound the system already breaks: a dependency
/// that was not installed then is expected to arrive at the right version, and
/// the backend may still install an older one — a pinned feed, or a container
/// that carries its own copy. The mismatch is reported rather than undone: the
/// containers are already on disk, and pica does not overrule the backend's
/// choice of version by downgrading it.
pub(super) fn recheck_constraints(
  app: &mut App,
  ctx: &InstallContext<'_>,
  precheck: &DependencyPrecheck,
) {
  for (group, constraints) in
    [("kmod", &precheck.deps.kmod), ("base", &precheck.deps.base), ("app", &precheck.deps.app)]
  {
    // Only required dependencies and the optional ones this install actually
    // put on the device: a declined one that happens to be installed was already
    // judged by the precheck, and warning about it again would blame this
    // install for a version it did not choose.
    let checked: Vec<Constraint> = constraints
      .iter()
      .filter(|dep| !dep.optional || precheck.optional.covers(group, &dep.name))
      .cloned()
      .collect();

    for message in crate::constraints::unmet(app, ctx.pkgname, group, &checked) {
      app.log_warn(message);
    }
  }
}
