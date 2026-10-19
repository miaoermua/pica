//! Optional dependencies: which ones the artifact declares with `?`, and which
//! the user asked for.
//!
//! A `?` declaration is optional, not a prerequisite. It goes through the
//! same precheck as everything else — that is the only way to know whether the
//! package exists and is already installed — but it never blocks the install, and
//! it only reaches a backend when the user chose it (or `--with-optional` did).
//!
//! Three rules shape this module:
//!
//! - The plan is built after the precheck and before the transaction, so it can be
//!   shown without having written anything, and a user who declines leaves no
//!   trace.
//! - Whatever state an item is in, the installer must be able to tell it apart.
//!   "Unavailable" is never silently turned into "declined", and a package that
//!   was already installed is never installed a second time.
//! - Nothing is guessed. Sizes come from the feed that would serve the package,
//!   for the version the backend would install; a feed that does not record a
//!   figure leaves it unknown rather than filling it in from the other one. The
//!   containers in `pkgs/` are one batch (see `feed::install_pkgs_dir`), so their
//!   members are not selectable one by one.

use super::feed::DependencySource;
use super::precheck::DeclaredDeps;
use crate::app::{App, OptionalDeps};
use crate::constraints::BOUND_UNMET;
use crate::pkgmgr::FeedCandidate;
use pica_pkg_core::{constraint, version};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{BufRead, IsTerminal, Write as IoWrite};

/// How one optional dependency can be treated by this install.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Availability {
  /// Already on the device. Shown, never reinstalled.
  Installed,
  /// In the backend's feeds and selectable.
  Available,
  /// Declared, but this install cannot deliver it. The reason is what the user
  /// sees, so it says why rather than "error".
  Unavailable(&'static str),
  /// Shipped in the artifact's `pkgs/` and installed with that batch. The batch
  /// is indivisible, so the item is shown and not selectable.
  Packaged,
}

/// One optional dependency, with what is known about it.
#[derive(Clone, Debug)]
pub(crate) struct Item {
  pub(crate) group: &'static str,
  pub(crate) name: String,
  pub(crate) requirement: Option<String>,
  pub(crate) availability: Availability,
  /// Whether the version already on the device failed the bound the artifact
  /// declares.
  ///
  /// This is not a kind of [`Availability`]: availability answers "can this
  /// install deliver the package", the bound answers "does the assertion still
  /// hold". They are orthogonal, for the same reason `required` and `optional`
  /// are two dimensions rather than one enum. A version that misses the bound is
  /// still installed and still out of this run's reach, so it stays
  /// [`Availability::Installed`]; only the wording and the report change.
  pub(crate) bound_unmet: bool,
  /// Compressed download size in bytes, when a trustworthy figure exists.
  pub(crate) download: Option<u64>,
  /// Installed size in bytes, when a trustworthy figure exists.
  pub(crate) installed_size: Option<u64>,
  pub(crate) selected: bool,
}

/// Everything one install decided about the artifact's optional dependencies.
#[derive(Debug, Default)]
pub(crate) struct Plan {
  items: Vec<Item>,
}

impl Plan {
  /// A plan for a backend that cannot install dependencies at all.
  pub(crate) fn unavailable(deps: &DeclaredDeps, reason: &'static str) -> Self {
    let mut items = Vec::new();
    for (group, constraints) in [("kmod", &deps.kmod), ("base", &deps.base), ("app", &deps.app)] {
      for dep in constraint::optional(constraints) {
        items.push(Item {
          group,
          name: dep.name.clone(),
          requirement: if dep.has_bound() { Some(dep.requirement()) } else { None },
          availability: Availability::Unavailable(reason),
          bound_unmet: false,
          download: None,
          installed_size: None,
          selected: false,
        });
      }
    }
    Self { items }
  }

  pub(crate) fn is_empty(&self) -> bool {
    self.items.is_empty()
  }

  /// Names of the optional dependencies the user chose for `group`.
  pub(crate) fn selected_names(&self, group: &str) -> Vec<String> {
    self
      .items
      .iter()
      .filter(|item| item.group == group && item.selected)
      .map(|item| item.name.clone())
      .collect()
  }

  pub(crate) fn selected_count(&self) -> usize {
    self.items.iter().filter(|item| item.selected).count()
  }

  /// Whether this install is responsible for `name`: it asked the backend for
  /// it, or the artifact's `pkgs/` batch installs it. Already-installed ones are
  /// excluded — the precheck judged those, and this run did not touch them.
  ///
  /// This is what decides which bounds are rechecked after step 5 (see
  /// `pipeline::recheck_constraints`). A packaged optional dependency is
  /// installed even though it was never selectable, so its bound has to be
  /// checked like any other package this run put on the device.
  pub(crate) fn covers(&self, group: &str, name: &str) -> bool {
    self.items.iter().any(|item| {
      item.group == group
        && item.name == name
        && (item.selected || item.availability == Availability::Packaged)
    })
  }

  /// States the optional dependencies this install will not deliver, once, so a user
  /// does not have to open the report to find out why.
  pub(crate) fn warn_skipped(&self, app: &mut App) {
    let mut by_reason: Vec<(&'static str, Vec<String>)> = Vec::new();
    for item in &self.items {
      let Availability::Unavailable(reason) = item.availability else {
        continue;
      };
      match by_reason.iter_mut().find(|(known, _)| *known == reason) {
        Some((_, names)) => names.push(item.name.clone()),
        None => by_reason.push((reason, vec![item.name.clone()])),
      }
    }

    for (reason, names) in by_reason {
      app.log_warn(format!("optional dependencies skipped ({}): {}", reason, names.join(" ")));
    }
  }

  /// The `optional` section of the install report.
  ///
  /// `selected_outcome` is what a chosen package became: `installed` for a
  /// completed install, `failed` for one that was rolled back. A declined or
  /// unavailable package is never recorded as installed.
  pub(crate) fn report(&self, selected_outcome: &str) -> Value {
    let items: Vec<Value> = self
      .items
      .iter()
      .map(|item| {
        let outcome = match item.availability {
          Availability::Installed => "installed_before",
          Availability::Available => {
            if item.selected {
              selected_outcome
            } else {
              "declined"
            }
          }
          Availability::Unavailable(_) => "unavailable",
          Availability::Packaged => "installed_with_pkgs",
        };

        let mut entry = json!({
          "group": item.group,
          "name": item.name,
          "optional": true,
          "outcome": outcome,
        });
        if let Some(requirement) = &item.requirement {
          entry["constraint"] = json!(requirement);
        }
        // `unavailable` says nothing about why, and "the feeds could not be
        // read" is a different answer from "the feeds do not have it". The text
        // is the `&'static str` the warning and the selector already print, so
        // the two cannot describe the same item differently.
        if let Availability::Unavailable(reason) = item.availability {
          entry["reason"] = json!(reason);
        }
        // `installed_before` alone would hide that the version pica found is not
        // the version the artifact asked for. Same key, same meaning as on the
        // precheck entry: present means the question was asked and answered no,
        // so only the negative is written; a satisfied bound needs no restating.
        if item.bound_unmet {
          entry["bound_satisfied"] = json!(false);
        }
        if let Some(size) = item.download {
          entry["download_size"] = json!(size);
        }
        if let Some(size) = item.installed_size {
          entry["installed_size"] = json!(size);
        }
        entry
      })
      .collect();

    Value::Array(items)
  }
}

/// Builds the plan from what the precheck already learned, then asks the user
/// for a choice when the mode and the terminal allow it.
pub(crate) fn plan(
  app: &mut App,
  package: &str,
  deps: &DeclaredDeps,
  report: &Value,
  source: Option<DependencySource>,
) -> Plan {
  let mut items = Vec::new();

  for (group, constraints) in [("kmod", &deps.kmod), ("base", &deps.base), ("app", &deps.app)] {
    for dep in constraint::optional(constraints) {
      let status = status_of(report, group, &dep.name);
      items.push(Item {
        group,
        name: dep.name.clone(),
        requirement: if dep.has_bound() { Some(dep.requirement()) } else { None },
        availability: availability(group, status.as_deref(), source),
        bound_unmet: bound_unmet_in(report, group, &dep.name),
        download: None,
        installed_size: None,
        selected: false,
      });
    }
  }

  // Sizes are only promised for what can actually be installed: a package
  // already on the device has nothing to download, and one in `pkgs/` is not a
  // feed file.
  let names: Vec<&str> = items
    .iter()
    .filter(|item| item.availability == Availability::Available)
    .map(|item| item.name.as_str())
    .collect();
  if !names.is_empty() {
    let offers = feed_offers(app, &names);
    for item in &mut items {
      let Some(offer) = offers.get(&item.name) else {
        continue;
      };
      item.download = offer.download;
      item.installed_size = offer.installed;
    }
  }

  let mut plan = Plan { items };
  if plan.items.is_empty() {
    return plan;
  }

  match app.options.optional_deps {
    OptionalDeps::None => {}
    // `--with-optional` is an explicit instruction, so it applies without a
    // terminal and without asking. Already-installed items are left alone.
    OptionalDeps::All => {
      for item in &mut plan.items {
        item.selected = item.availability == Availability::Available;
      }
    }
    OptionalDeps::Ask => ask(app, package, &mut plan),
  }

  plan
}

/// Shows the selector and reads the answer.
///
/// The prompt is written to stderr, never stdout: `--json` may be on, and the
/// greeting is not the result of the command.
fn ask(app: &App, package: &str, plan: &mut Plan) {
  if !plan.items.iter().any(|item| item.availability == Availability::Available) {
    return;
  }

  // `--non-interactive` is the documented way to say "do not wait"; a stdin that
  // is not a terminal is the same situation without the flag. Reading either
  // would block a cron job or swallow a script's input.
  if app.options.non_interactive || !std::io::stdin().is_terminal() {
    if !app.options.non_interactive {
      eprintln!(
        "pica: stdin is not a terminal; installing no optional dependencies \
         (use --with-optional to include them)"
      );
    }
    return;
  }

  eprint!("{}", render(package, plan));
  let selectable: Vec<usize> = plan
    .items
    .iter()
    .enumerate()
    .filter_map(|(index, item)| (item.availability == Availability::Available).then_some(index + 1))
    .collect();

  loop {
    eprint!("Select optional packages by number, `all`, or press Enter to skip: ");
    let _ = std::io::stderr().flush();

    let mut input = String::new();
    let stdin = std::io::stdin();
    // EOF means nobody is there to answer, which is a skip, not an error.
    if stdin.lock().read_line(&mut input).unwrap_or(0) == 0 {
      eprintln!();
      return;
    }

    match parse_selection(&input, &selectable) {
      Ok(chosen) => {
        for index in chosen {
          plan.items[index - 1].selected = true;
        }
        return;
      }
      Err(message) => eprintln!("{message}"),
    }
  }
}

/// Renders the list shown before the question.
///
/// Kept separate from the prompt so the formatting can be asserted without a
/// terminal, and so the text cannot accidentally go to stdout.
pub(crate) fn render(package: &str, plan: &Plan) -> String {
  let mut out = format!("Optional dependencies for {package}\n\n");
  let width = plan.items.iter().map(|item| item.name.len()).max().unwrap_or(0);

  for (index, item) in plan.items.iter().enumerate() {
    let (mark, detail) = match item.availability {
      Availability::Installed => ("[+]", installed_detail(item)),
      Availability::Packaged => {
        ("[+]", "installed from the artifact's pkgs/ with this request".to_string())
      }
      Availability::Unavailable(reason) => ("[-]", format!("unavailable ({reason})")),
      Availability::Available => {
        let mark = if item.selected { "[x]" } else { "[ ]" };
        (mark, sizes(item))
      }
    };
    let _ = writeln!(out, "  [{}] {mark} {:<width$}  {detail}", index + 1, item.name);
  }

  if plan.items.iter().any(|item| item.download.is_some() || item.installed_size.is_some()) {
    out.push_str(
      "\nSizes are this package alone, as its feed records them; they exclude dependencies.\n",
    );
  }
  out
}

/// The detail line for an already-installed item.
///
/// A version that misses the declared bound is said so here: the log already
/// warns about it during the precheck, and calling the same item "already
/// installed" without qualification would contradict that warning on the same
/// screen. The bound text is the artifact's own declaration; the installed
/// version is not printed because this module never asked which version that
/// is, and one extra question just to decorate a line would bring its own
/// failure mode.
fn installed_detail(item: &Item) -> String {
  if !item.bound_unmet {
    return "already installed".to_string();
  }
  match &item.requirement {
    Some(requirement) => format!("already installed, but {BOUND_UNMET} ({requirement})"),
    None => format!("already installed, but {BOUND_UNMET}"),
  }
}

fn sizes(item: &Item) -> String {
  let download = item.download.map_or_else(|| "Unknown".to_string(), format_bytes);
  let installed = item
    .installed_size
    .map_or_else(|| "Unknown".to_string(), |bytes| format!("~{} (est.)", format_bytes(bytes)));
  format!("Download: {download}  Installed: {installed}")
}

/// Parses one line of the selector into the 1-based numbers it names.
///
/// Empty input is a skip. `all` is every offered number. Anything else must be
/// a number from the list: a typo is reported instead of being treated as "no",
/// because silently installing nothing after a typo is the worse surprise.
pub(crate) fn parse_selection(input: &str, selectable: &[usize]) -> Result<Vec<usize>, String> {
  let trimmed = input.trim();
  if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none") {
    return Ok(Vec::new());
  }
  if trimmed.eq_ignore_ascii_case("all") {
    return Ok(selectable.to_vec());
  }

  let mut chosen = Vec::new();
  for token in trimmed.split([' ', ',', '\t']) {
    if token.is_empty() {
      continue;
    }
    let number: usize =
      token.parse().map_err(|_| format!("`{token}` is not a number; nothing was selected"))?;
    if !selectable.contains(&number) {
      return Err(format!("`{token}` is not one of the offered numbers; nothing was selected"));
    }
    if !chosen.contains(&number) {
      chosen.push(number);
    }
  }
  Ok(chosen)
}

#[allow(clippy::cast_precision_loss, reason = "a byte count is far below 2^52")]
fn format_bytes(bytes: u64) -> String {
  const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
  let mut value = bytes as f64;
  let mut unit = 0;
  while value >= 1024.0 && unit + 1 < UNITS.len() {
    value /= 1024.0;
    unit += 1;
  }
  if unit == 0 { format!("{bytes} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

/// The precheck entry for one declaration, if the precheck looked at it.
fn entry_of<'a>(report: &'a Value, group: &str, name: &str) -> Option<&'a Value> {
  report
    .get(group)?
    .as_array()?
    .iter()
    .find(|entry| entry.get("name").and_then(Value::as_str) == Some(name))
}

/// The precheck status of one declaration, if the precheck looked at it.
fn status_of(report: &Value, group: &str, name: &str) -> Option<String> {
  entry_of(report, group, name)
    .and_then(|entry| entry.get("status"))
    .and_then(Value::as_str)
    .map(ToString::to_string)
}

/// Whether the precheck compared an installed version against a bound and found
/// it wanting.
///
/// A missing key is "the question was not asked" — no bound, or nothing
/// installed — so it reads as `false` without claiming the bound held.
fn bound_unmet_in(report: &Value, group: &str, name: &str) -> bool {
  entry_of(report, group, name)
    .and_then(|entry| entry.get("bound_satisfied"))
    .and_then(Value::as_bool)
    == Some(false)
}

/// Turns a precheck status plus the artifact's dependency source into what this
/// install can offer.
fn availability(
  group: &str,
  status: Option<&str>,
  source: Option<DependencySource>,
) -> Availability {
  match status {
    Some("installed") => Availability::Installed,
    // Kernel modules always come from the feeds: an application artifact cannot
    // carry one, so the containers decision does not apply to them.
    Some("feed") if group == "kmod" => Availability::Available,
    Some("feed") => match source {
      Some(DependencySource::Feeds) => Availability::Available,
      Some(DependencySource::Containers) => {
        Availability::Unavailable("this artifact installs its dependencies from pkgs/")
      }
      None => Availability::Unavailable("this install has no dependency source"),
    },
    Some("packaged") => match source {
      Some(DependencySource::Containers) => Availability::Packaged,
      _ => Availability::Unavailable("packaged in the artifact, but this install uses the feeds"),
    },
    Some("unknown") => Availability::Unavailable("the feeds could not be read"),
    _ => Availability::Unavailable("not available from the feeds"),
  }
}

/// The newest version of each offered package, with the sizes its feed recorded.
///
/// The newest is the one the backend would install: `opkg` and `apk` both pick
/// the highest version when given a bare name, so any other version's figures
/// would describe a package this install is not going to put on the device.
///
/// A feed figure is not an invoice: `download` is the package file and
/// `installed` the unpacked size it declares, and either may be missing. They
/// are reported separately and never derived from each other.
fn feed_offers(app: &mut App, names: &[&str]) -> HashMap<String, FeedCandidate> {
  let mut offers = HashMap::new();
  let mut failure = None;

  if let Ok(package_manager) = app.package_manager() {
    for name in names {
      match package_manager.feed_candidates(name) {
        Ok(candidates) => {
          if let Some(newest) = newest_offer(candidates) {
            offers.insert((*name).to_string(), newest);
          }
        }
        Err(err) => failure = Some(err),
      }
    }
  }

  if let Some(err) = failure {
    // The figures are an extra, not a prerequisite: an index that stopped
    // answering after the precheck leaves the install possible with the sizes
    // unknown, which the selector already knows how to say.
    app.log_warn(format!("optional dependency sizes are unknown: {}", err.message));
  }
  offers
}

/// The candidate a bare name installs: the highest version, by the same order
/// the bound checks use.
fn newest_offer(candidates: Vec<FeedCandidate>) -> Option<FeedCandidate> {
  candidates.into_iter().max_by(|a, b| version::pkgver_cmp(&a.version, &b.version))
}

#[cfg(test)]
mod tests {
  use super::*;
  use pica_pkg_core::constraint::Constraint;

  fn item(name: &str, availability: Availability) -> Item {
    Item {
      group: "app",
      name: name.to_string(),
      requirement: None,
      availability,
      bound_unmet: false,
      download: None,
      installed_size: None,
      selected: false,
    }
  }

  #[test]
  fn a_met_bound_keeps_the_plain_installed_line() {
    let plan = Plan {
      items: vec![Item {
        requirement: Some(">=1.0".to_string()),
        ..item("present", Availability::Installed)
      }],
    };
    let text = render("demo", &plan);
    assert!(text.contains("already installed"), "{text}");
    assert!(!text.contains(BOUND_UNMET), "{text}");
    let entry = &plan.report("installed")[0];
    assert!(entry.get("reason").is_none(), "{entry}");
    assert!(entry.get("bound_satisfied").is_none(), "a satisfied bound is not restated: {entry}");
  }

  #[test]
  fn an_unmet_bound_is_said_once_and_left_alone() {
    let plan = Plan {
      items: vec![Item {
        requirement: Some(">=2.0".to_string()),
        bound_unmet: true,
        ..item("present", Availability::Installed)
      }],
    };

    let text = render("demo", &plan);
    let line = text.lines().find(|line| line.contains("present")).expect("the item is listed");
    assert!(line.contains(BOUND_UNMET), "{line}");
    assert!(line.contains(">=2.0"), "the declared constraint is named: {line}");
    assert_eq!(
      text.lines().filter(|line| line.contains(BOUND_UNMET)).count(),
      1,
      "the detail stays on the item's own line: {text}"
    );

    let report = plan.report("installed");
    assert_eq!(report[0]["outcome"], "installed_before", "{report}");
    assert_eq!(report[0]["bound_satisfied"], false, "{report}");
    assert!(
      report[0].get("reason").is_none(),
      "`reason` explains an unavailable outcome only: {report}"
    );

    // Missing the bound is not a reason to touch the package: it is still not
    // selected, and this install still does not cover it.
    assert_eq!(plan.selected_names("app"), Vec::<String>::new());
    assert_eq!(plan.selected_count(), 0);
    assert!(!plan.covers("app", "present"));
  }

  #[test]
  fn a_missing_verdict_is_not_a_failed_bound() {
    // The precheck omits `bound_satisfied` when there was nothing to compare:
    // no bound declared, or nothing installed. A missing key is not `false`.
    let no_bound = json!({"app": [{"name": "plain", "status": "installed"}]});
    assert!(!bound_unmet_in(&no_bound, "app", "plain"));

    let met = json!({
      "app": [{"name": "ok", "status": "installed", "constraint": ">=1.0", "bound_satisfied": true}]
    });
    assert!(!bound_unmet_in(&met, "app", "ok"));

    let unmet = json!({
      "app": [{"name": "old", "status": "installed", "constraint": ">=2.0", "bound_satisfied": false}]
    });
    assert!(bound_unmet_in(&unmet, "app", "old"));

    // With no verdict the screen reads exactly as it did before this field
    // existed, and the report gains nothing.
    let plan = Plan { items: vec![item("plain", Availability::Installed)] };
    let text = render("demo", &plan);
    assert!(text.contains("already installed"), "{text}");
    assert!(!text.contains(BOUND_UNMET), "{text}");
    let entry = &plan.report("installed")[0];
    assert!(entry.get("reason").is_none(), "{entry}");
    assert!(entry.get("bound_satisfied").is_none(), "{entry}");
  }

  #[test]
  fn an_unknown_size_is_not_shown_as_zero() {
    // A feed that records 0 means "not measured" (apk's ADB containers), and
    // both readers turn it into `None`: `opkg.rs`'s `candidate` filters
    // `*bytes > 0`, and `apk.rs`'s `apk_size` does the same. An absent figure
    // has to read as unknown, not 0.
    let plan = Plan { items: vec![item("xray-core", Availability::Available)] };
    let text = render("demo", &plan);
    assert!(text.contains("Download: Unknown"), "{text}");
    assert!(text.contains("Installed: Unknown"), "{text}");
    assert!(!text.contains(" 0 B"), "{text}");
  }

  #[test]
  fn a_known_size_is_marked_as_an_estimate() {
    let plan = Plan {
      items: vec![Item {
        installed_size: Some(4 * 1024 * 1024),
        ..item("xray-core", Availability::Available)
      }],
    };
    let text = render("demo", &plan);
    assert!(text.contains("Installed: ~4.0 MiB (est.)"), "{text}");
    assert!(text.contains("exclude dependencies"), "{text}");
  }

  #[test]
  fn both_feed_sizes_are_shown_and_kept_apart() {
    let plan = Plan {
      items: vec![Item {
        download: Some(8192),
        installed_size: Some(40960),
        ..item("xray-core", Availability::Available)
      }],
    };
    let text = render("demo", &plan);
    assert!(text.contains("Download: 8.0 KiB"), "{text}");
    assert!(text.contains("Installed: ~40.0 KiB (est.)"), "{text}");
    assert!(text.contains("exclude dependencies"), "{text}");
  }

  #[test]
  fn the_newest_offer_is_the_one_shown() {
    // `1.10` is newer than `1.9`, which a string comparison would get wrong.
    let candidates = vec![
      FeedCandidate { version: "1.9-r1".to_string(), download: Some(100), installed: Some(1000) },
      FeedCandidate { version: "1.10-r1".to_string(), download: Some(200), installed: Some(2000) },
      FeedCandidate { version: "1.2-r1".to_string(), download: Some(50), installed: None },
    ];
    assert_eq!(
      newest_offer(candidates),
      Some(FeedCandidate {
        version: "1.10-r1".to_string(),
        download: Some(200),
        installed: Some(2000),
      }),
      "the backend installs the highest version, so its figures are the ones to show"
    );
    assert_eq!(newest_offer(Vec::new()), None, "a feed with no version offers nothing");
  }

  #[test]
  fn the_report_records_the_two_sizes_apart() {
    let plan = Plan {
      items: vec![Item {
        selected: true,
        download: Some(8192),
        installed_size: Some(40960),
        ..item("chosen", Availability::Available)
      }],
    };
    let report = plan.report("installed");
    assert_eq!(report[0]["download_size"], 8192);
    assert_eq!(report[0]["installed_size"], 40960);
  }

  #[test]
  fn every_state_has_its_own_marker() {
    let plan = Plan {
      items: vec![
        item("offered", Availability::Available),
        item("present", Availability::Installed),
        item("bundled", Availability::Packaged),
        item("absent", Availability::Unavailable("not available from the feeds")),
      ],
    };
    let text = render("demo", &plan);
    assert!(text.contains("[1] [ ] offered"), "{text}");
    assert!(text.contains("[2] [+] present"), "{text}");
    assert!(text.contains("already installed"), "{text}");
    assert!(text.contains("installed from the artifact's pkgs/ with this request"), "{text}");
    assert!(text.contains("[4] [-] absent"), "{text}");
    assert!(text.contains("unavailable (not available from the feeds)"), "{text}");
    assert!(text.starts_with("Optional dependencies for demo\n"), "{text}");
  }

  #[test]
  fn a_selection_is_numbers_or_all_and_typos_are_reported() {
    let offered = [1, 3];
    assert_eq!(parse_selection("", &offered), Ok(Vec::new()));
    assert_eq!(parse_selection("  none ", &offered), Ok(Vec::new()));
    assert_eq!(parse_selection("1, 3", &offered), Ok(vec![1, 3]));
    assert_eq!(parse_selection("3 1 3", &offered), Ok(vec![3, 1]));
    assert_eq!(parse_selection("all", &offered), Ok(vec![1, 3]));
    assert!(parse_selection("2", &offered).is_err());
    assert!(parse_selection("xray", &offered).is_err());
  }

  #[test]
  fn the_report_keeps_declined_unavailable_and_installed_apart() {
    let plan = Plan {
      items: vec![
        Item { selected: true, ..item("chosen", Availability::Available) },
        item("declined", Availability::Available),
        item("present", Availability::Installed),
        item("bundled", Availability::Packaged),
        item("absent", Availability::Unavailable("not available from the feeds")),
      ],
    };

    let report = plan.report("installed");
    let outcomes: Vec<&str> =
      report.as_array().expect("array").iter().filter_map(|e| e["outcome"].as_str()).collect();
    assert_eq!(
      outcomes,
      vec!["installed", "declined", "installed_before", "installed_with_pkgs", "unavailable"]
    );
    assert_eq!(plan.report("failed")[0]["outcome"], "failed");
  }

  #[test]
  fn an_unavailable_item_records_the_same_reason_the_screen_shows() {
    // "The feeds could not be read" and "the feeds do not have it" both end up
    // as `unavailable`; only the reason tells them apart, so the report has to
    // keep it and keep the two different.
    let plan = Plan {
      items: vec![
        item("unread", Availability::Unavailable("the feeds could not be read")),
        item("absent", Availability::Unavailable("not available from the feeds")),
        item("present", Availability::Installed),
      ],
    };

    let report = plan.report("installed");
    let text = render("demo", &plan);
    for name in ["unread", "absent"] {
      let entry = report
        .as_array()
        .expect("array")
        .iter()
        .find(|entry| entry["name"] == name)
        .expect("the item is reported");
      let reason = entry["reason"].as_str().expect("an unavailable item has a reason");
      assert!(text.contains(reason), "the screen says {reason}: {text}");
    }
    assert_ne!(report[0]["reason"], report[1]["reason"], "the two failures stay apart");
    assert!(report[2].get("reason").is_none(), "installed items gain nothing: {}", report[2]);
  }

  #[test]
  fn reason_and_bound_satisfied_never_share_an_entry() {
    // `reason` explains an `unavailable` outcome; `bound_satisfied` records that
    // an installed version missed its bound. An entry is one or the other, and
    // an entry with neither needs no explaining.
    let plan = Plan {
      items: vec![
        item("absent", Availability::Unavailable("not available from the feeds")),
        Item {
          requirement: Some(">=2.0".to_string()),
          bound_unmet: true,
          ..item("old", Availability::Installed)
        },
        item("present", Availability::Installed),
        item("offered", Availability::Available),
      ],
    };

    let report = plan.report("installed");
    let entries = report.as_array().expect("array");

    assert_eq!(entries[0]["outcome"], "unavailable");
    assert_eq!(entries[0]["reason"], "not available from the feeds");
    assert!(entries[0].get("bound_satisfied").is_none(), "{}", entries[0]);

    assert_eq!(entries[1]["outcome"], "installed_before");
    assert_eq!(entries[1]["bound_satisfied"], false, "{}", entries[1]);
    assert!(entries[1].get("reason").is_none(), "{}", entries[1]);

    for entry in &entries[2..] {
      assert!(entry.get("reason").is_none(), "{entry}");
      assert!(entry.get("bound_satisfied").is_none(), "{entry}");
    }
  }

  #[test]
  fn an_unreadable_index_is_a_different_answer_from_an_absent_package() {
    // The precheck records `unknown` when it could not read the index and
    // `missing` when the index said no. Both are unavailable, but they are not
    // the same fact, so the reason keeps them apart.
    let feeds = Some(DependencySource::Feeds);
    assert_eq!(
      availability("app", Some("unknown"), feeds),
      Availability::Unavailable("the feeds could not be read")
    );
    assert_eq!(
      availability("app", Some("missing"), feeds),
      Availability::Unavailable("not available from the feeds")
    );
  }

  #[test]
  fn the_pica_path_states_its_reason_too() {
    let deps = DeclaredDeps {
      kmod: Vec::new(),
      base: Vec::new(),
      app: vec![Constraint::parse("opt-app?").expect("valid declaration")],
    };
    let reason = "pkgmgr=pica cannot install dependencies";
    let plan = Plan::unavailable(&deps, reason);

    let entry = &plan.report("installed")[0];
    assert_eq!(entry["outcome"], "unavailable");
    assert_eq!(entry["reason"], reason, "{entry}");
    assert!(render("demo", &plan).contains(reason));
  }

  #[test]
  fn only_the_choices_that_can_be_installed_are_installed() {
    let plan = Plan {
      items: vec![
        Item { selected: true, ..item("chosen", Availability::Available) },
        item("absent", Availability::Unavailable("not available from the feeds")),
        item("present", Availability::Installed),
      ],
    };
    assert_eq!(plan.selected_names("app"), vec!["chosen".to_string()]);
    assert_eq!(plan.selected_count(), 1);
    assert!(plan.covers("app", "chosen"));
    assert!(!plan.covers("app", "absent"));
  }

  #[test]
  fn a_packaged_optional_dependency_is_covered_even_though_it_is_not_selectable() {
    // It goes on the device with the `pkgs/` batch, so this run is the one that
    // has to stand behind a bound it declared.
    let plan = Plan {
      items: vec![
        item("bundled", Availability::Packaged),
        item("present", Availability::Installed),
      ],
    };
    assert!(plan.covers("app", "bundled"));
    assert!(!plan.covers("app", "present"));
    assert!(plan.selected_names("app").is_empty());
  }

  #[test]
  fn a_declined_optional_dependency_is_not_declared_missing() {
    // The contract with the precheck: `?` entries never appear in the missing
    // list, so the install continues when they are absent.
    let report = json!({
      "kmod": [],
      "base": [],
      "app": [
        {"name": "optional", "status": "missing", "optional": true},
        {"name": "required", "status": "missing"},
      ],
    });
    assert_eq!(
      super::super::precheck::summarize_missing_precheck(&report),
      vec!["app:required".to_string()]
    );
  }
}
