mod app;
mod candidate;
mod commands;
mod constraints;
mod format;
mod i18n;
mod installed;
mod lock;
mod menu;
mod pkgmgr;
mod platform;
mod source;
mod state;
mod system;

use crate::app::{
  App, CliError, CliResult, E_ARG_INVALID, FeedPolicy, JsonMode, OptionalDeps, Options, Paths,
  conf_get_language, parse_options, require_arg,
};
use crate::commands::{install, query, remove, search, setup, sync, upgrade};
use crate::i18n::tr;
use crate::i18n::{Key, Lang};
use crate::lock::LockGuard;
use crate::state::cleanup_pkg_cache;
use crate::system::need_cmd;
use pica_pkg_core::PICA_VERSION;
use std::env;
use std::process;

fn usage(app: &App) {
  println!("{}", app.text(Key::Usage));
}

fn main() {
  let paths = Paths::from_env();

  let raw_args: Vec<String> = env::args().skip(1).collect();
  // The language is needed before the options are parsed, so a mistake in them
  // is reported in it too.
  early_lang(&raw_args, &paths).make_current();
  let (options, args) = match parse_options(&raw_args) {
    Ok(value) => value,
    Err(err) => {
      let app = App::new(
        paths,
        Options {
          json_mode: JsonMode::None,
          non_interactive: false,
          feed_policy: FeedPolicy::Ask,
          fetch_timeout: 30,
          fetch_retry: 2,
          fetch_retry_delay: 1,
          pkgmgr: None,
          optional_deps: OptionalDeps::Ask,
          lang: None,
          dry_run: false,
          noconfirm: false,
        },
      );
      app.emit_error(&err);
      process::exit(1);
    }
  };

  let mut app = App::new(paths, options);
  if let Err(err) = app.apply_backend_config() {
    app.emit_error(&err);
    process::exit(1);
  }

  if args.is_empty() {
    usage(&app);
    process::exit(2);
  }

  let command = args[0].as_str();
  if matches!(command, "-h" | "--help" | "help") {
    usage(&app);
    app.emit_success(command, "usage");
    return;
  }
  if matches!(command, "-V" | "--version") {
    println!("{PICA_VERSION}");
    app.emit_success(command, PICA_VERSION);
    return;
  }

  let lock_guard = match LockGuard::acquire(&app.paths.lock_file, app.lang) {
    Ok(guard) => guard,
    Err(err) => {
      app.emit_error(&err);
      process::exit(1);
    }
  };

  let result = run_command(&mut app, &args);
  cleanup_pkg_cache(&mut app);
  drop(lock_guard);

  match result {
    Ok((cmd, target)) => app.emit_success(cmd, &target),
    Err(err) => {
      app.emit_error(&err);
      process::exit(1);
    }
  }
}

/// The language of this run as far as it can be known before parsing: an
/// explicit `--lang`, else the configured one.
fn early_lang(args: &[String], paths: &Paths) -> Lang {
  args
    .iter()
    .position(|arg| arg == "--lang" || arg == "--language")
    .and_then(|at| args.get(at + 1))
    .and_then(|value| Lang::parse(value))
    .unwrap_or_else(|| conf_get_language(&paths.conf_file))
}

/// Refuses arguments a command would otherwise drop without a word: `-S a b`
/// must not install `a` and report success.
fn reject_extra_args(app: &App, args: &[String]) -> CliResult<()> {
  let accepted = match args[0].as_str() {
    "setup" | "-Su" | "-Syu" | "-Q" | "-Qu" => 1,
    "-Qi" | "-Ql" | "-Qo" | "-So" | "-Si" => 2,
    // `-Sl [source] [--all]`.
    "-Sl" => 3,
    // Search terms are a phrase, every word has to match; `-S`, `-Sp`, `-U`
    // and `-R` take any number of targets.
    _ => return Ok(()),
  };
  match args.get(accepted) {
    Some(extra) => {
      // `accepted` counts the command itself, so 1 means the command carries no
      // operand at all. Telling a user that `-Q` "takes one argument" right
      // after they passed one reads as if it wanted it.
      let key = if accepted == 1 { Key::UnexpectedArgument } else { Key::TooManyArguments };
      Err(CliError::new(E_ARG_INVALID, app.args(key, &[&args[0], extra])))
    }
    None => Ok(()),
  }
}

/// The words after a search command, joined, and whether `--all` was among
/// them.
fn search_terms(args: &[String]) -> (String, bool) {
  let all = args.iter().skip(1).any(|arg| arg == "--all");
  let terms: Vec<&str> =
    args.iter().skip(1).map(String::as_str).filter(|arg| *arg != "--all").collect();
  (terms.join(" "), all)
}

/// The read-only search commands, or `None` for any other command.
fn run_search(app: &mut App, args: &[String]) -> Option<CliResult<(&'static str, String)>> {
  let command: &'static str = match args[0].as_str() {
    "-Ss" => "-Ss",
    "-Sl" => "-Sl",
    "-Qs" => "-Qs",
    "-Qo" => "-Qo",
    "-Qu" => "-Qu",
    _ => return None,
  };
  app.set_phase("query");
  let (terms, all) = search_terms(args);
  let result = match command {
    "-Ss" => search::sync(app, &terms, all),
    "-Qs" => search::installed(app, &terms),
    "-Qu" => query::upgradable(app),
    "-Qo" => match require_arg(args, 1, &tr(Key::RequiresArgument, &[&"-Qo", &"<path>"])) {
      Ok(path) => query::owner(app, path),
      Err(err) => Err(err),
    },
    _ => search::list(app, (!terms.is_empty()).then_some(terms.as_str()), all),
  };
  Some(result.map(|()| (command, terms)))
}

/// Refuses `--dry-run` where there is no change to preview: `setup` asks its
/// own questions, and a sync only refreshes the index.
fn reject_dry_run(app: &App, args: &[String]) -> CliResult<()> {
  let previews = match args[0].as_str() {
    "setup" => false,
    "-S" => args.len() > 1,
    _ => true,
  };
  if app.options.dry_run && !previews {
    let what = if args[0] == "-S" { "-S (sync)" } else { args[0].as_str() };
    return Err(CliError::new(E_ARG_INVALID, app.args(Key::DryRunNotApplicable, &[what])));
  }
  Ok(())
}

/// Runs `each` over every target.
///
/// One target is the command it always was. Several are listed and confirmed
/// once, then run one at a time so a failure stays that target's, and the run
/// ends with a summary. A dry run previews each of them instead of asking.
fn run_targets(
  app: &mut App,
  targets: &[String],
  each: fn(&mut App, &str) -> CliResult<()>,
) -> CliResult<String> {
  if let [only] = targets {
    each(app, only)?;
    return Ok(only.clone());
  }

  app.info_args(Key::BatchTargets, &[&targets.len().to_string(), &targets.join(" ")]);
  if !app.options.dry_run {
    app.confirm()?;
    app.confirmed = true;
  }
  let mut failed = Vec::new();
  for target in targets {
    if let Err(err) = each(app, target) {
      app.warn_args(Key::CommandFailed, &[target, &err.message]);
      failed.push((target.as_str(), err));
    }
  }

  let succeeded = (targets.len() - failed.len()).to_string();
  app.info_args(Key::BatchSummary, &[&succeeded, &failed.len().to_string()]);
  match failed.first() {
    Some((_, first)) => {
      let names: Vec<&str> = failed.iter().map(|(name, _)| *name).collect();
      Err(CliError::new(first.code, app.args(Key::BatchSomeFailed, &[&names.join(", ")])))
    }
    None => Ok(targets.join(" ")),
  }
}

fn run_command(app: &mut App, args: &[String]) -> CliResult<(&'static str, String)> {
  reject_extra_args(app, args)?;
  reject_dry_run(app, args)?;
  if let Some(result) = run_search(app, args) {
    return result;
  }
  let command = args[0].as_str();
  match command {
    "setup" => {
      app.set_phase("setup");
      setup::run(app)?;
      Ok(("setup", "configured".to_string()))
    }
    "-S" => {
      if args.len() > 1 {
        app.set_phase("install");
        let targets = run_targets(app, &args[1..], install::app_auto)?;
        Ok(("-S", targets))
      } else {
        app.set_phase("sync");
        sync::repos(app)?;
        Ok(("-S", "repos".to_string()))
      }
    }
    "-Su" => {
      app.set_phase("upgrade");
      upgrade::all(app)?;
      Ok(("-Su", "all".to_string()))
    }
    "-Syu" => {
      app.set_phase("sync");
      sync::repos(app)?;
      app.set_phase("upgrade");
      upgrade::all(app)?;
      Ok(("-Syu", "all".to_string()))
    }
    "-Q" => {
      app.set_phase("query");
      query::installed(app)?;
      Ok(("-Q", "installed".to_string()))
    }
    "-Qi" => {
      app.set_phase("query");
      let pkgname = require_arg(args, 1, &tr(Key::RequiresArgument, &[&"-Qi", &"<pkgname>"]))?;
      query::info(app, pkgname)?;
      Ok(("-Qi", pkgname.to_string()))
    }
    "-Ql" => {
      app.set_phase("query");
      let pkgname = require_arg(args, 1, &tr(Key::RequiresArgument, &[&"-Ql", &"<pkgname>"]))?;
      query::files(app, pkgname)?;
      Ok(("-Ql", pkgname.to_string()))
    }
    "-So" => {
      app.set_phase("install");
      let selector = require_arg(args, 1, &tr(Key::RequiresArgument, &[&"-So", &"<selector>"]))?;
      install::app_via_package_manager(app, selector)?;
      Ok(("-So", selector.to_string()))
    }
    "-Si" => {
      app.set_phase("query");
      let selector = require_arg(args, 1, &tr(Key::RequiresArgument, &[&"-Si", &"<selector>"]))?;
      query::sync_info(app, selector)?;
      Ok(("-Si", selector.to_string()))
    }
    "-Sp" => {
      app.set_phase("install");
      need_cmd("tar")?;
      require_arg(args, 1, &tr(Key::RequiresArgument, &[&"-Sp", &"<selector>"]))?;
      let targets = run_targets(app, &args[1..], install::pica_from_repo)?;
      Ok(("-Sp", targets))
    }
    "-U" => {
      app.set_phase("install");
      need_cmd("tar")?;
      require_arg(args, 1, &tr(Key::RequiresArgument, &[&"-U", &"<pkgfile|url>"]))?;
      let targets =
        run_targets(app, &args[1..], |app, source| install::pkg_source(app, source, None))?;
      Ok(("-U", targets))
    }
    "-R" => {
      app.set_phase("remove");
      require_arg(args, 1, &tr(Key::RequiresArgument, &[&"-R", &"<pkgname>"]))?;
      let targets = run_targets(app, &args[1..], remove::pkg)?;
      Ok(("-R", targets))
    }
    other => Err(CliError::new(E_ARG_INVALID, tr(Key::UnknownArg, &[&other]))),
  }
}
