mod commands;
mod helpers;
mod lock;
mod state;
mod system;
mod types;

use crate::commands::{install, query, remove, sync, upgrade};
use crate::helpers::{
  build_precheck_report, canonicalize_display, cleanup_pkg_cache_with_notice, conf_get_i18n,
  copy_dir_recursive, detect_luci_variant, detect_opkg_arches, detect_os, detect_platform,
  ensure_dir, find_pica_candidates_in_index, has_command, install_via_feeds_or_ipk, make_temp_dir,
  manifest_get_array, manifest_get_scalar, need_cmd, normalize_uname, pkg_list_diff_added,
  reorder_app_list, required_manifest_field, resolve_lang, run_hook, write_file_atomic,
};
use crate::lock::LockGuard;
use crate::state::read_json_file;
use crate::types::{
  ensure_dirs, parse_options, require_arg, App, CliError, CliResult, FeedPolicy, JsonMode, Options,
  Paths, E_ARG_INVALID, E_CONFIG_INVALID, E_DB_INVALID, E_INDEX_INVALID, E_INTEGRITY_INVALID, E_IO,
  E_JSON_INVALID, E_LOCK_BUSY, E_MANIFEST_INVALID, E_MISSING_COMMAND, E_NO_SPACE, E_OPKG_INSTALL,
  E_OPKG_REMOVE, E_PACKAGE_INVALID, E_PLATFORM_UNSUPPORTED, E_POLICY_INVALID, E_REPO_INVALID,
  E_RUNTIME, E_VERSION_INCOMPATIBLE,
};
use pica_pkg_core::manifest::{get_first as manifest_get_first, Manifest};
use pica_pkg_core::repo::is_supported_url;
use pica_pkg_core::selector::Selector;
use pica_pkg_core::version::{pkgver_cmp_key, pkgver_ge, ver_ge};
use pica_pkg_core::PICA_VERSION;
use std::env;
use std::process::{self};

fn usage() {
  println!(
        "Usage:\n  pica -S [selector]      Sync repos (no selector) or install by selector (auto)\n  pica -Su                Upgrade all installed pica packages\n  pica -Syu               Sync, then upgrade all installed pica packages\n  pica -Si <selector>     Show remote package info from synced index\n  pica -So <selector>     Install by selector (force opkg)\n  pica -Sp <selector>     Install by selector (force pica repo)\n  pica -U <pkgfile|url>   Install/Update from local file or URL\n  pica -R <pkgname>       Remove package (no dependency handling)\n  pica -Q                 List installed pica packages\n  pica -Qi <pkgname>      Show installed package info\n  pica -Ql <pkgname>      List installed package files\n  pica --json ...         Emit JSON on success and error (explicit only)\n  pica --json-errors ...  Emit JSON only on error\n  pica --non-interactive ...\n                            Disable prompts (for backend/automation)\n  pica --feed-policy <mode>\n                            ask|feed-first|packaged-first|feed-only|packaged-only\n  pica -V\n  pica --version\n\nNotes:\n  - Requires: opkg, tar, and one fetcher (uclient-fetch/wget/curl) for URL install/sync.\n  - Config: /etc/pica/pica.json\n  - State:  /var/lib/pica/db.json, /var/lib/pica/index.json\n  - Lock:   /var/lib/pica/db.lck\n  - Selector example: app(branch)"
    );
}

fn main() {
  let paths = Paths::from_env();

  let (options, args) = match parse_options(env::args().skip(1).collect()) {
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
        },
      );
      app.emit_error(&err);
      process::exit(1);
    }
  };

  let mut app = App::new(paths, options);

  if args.is_empty() {
    usage();
    process::exit(2);
  }

  let command = args[0].as_str();
  if matches!(command, "-h" | "--help" | "help") {
    usage();
    app.emit_success(command, "usage");
    return;
  }
  if matches!(command, "-V" | "--version") {
    println!("{PICA_VERSION}");
    app.emit_success(command, PICA_VERSION);
    return;
  }

  if app.options.json_mode != JsonMode::None && !has_command("jq") {
    let err = CliError::new(E_MISSING_COMMAND, "--json/--json-errors requires command: jq");
    app.emit_error(&err);
    process::exit(1);
  }

  let lock_guard = match LockGuard::acquire(&app.paths.lock_file) {
    Ok(guard) => guard,
    Err(err) => {
      app.emit_error(&err);
      process::exit(1);
    }
  };

  let result = run_command(&mut app, &args);
  cleanup_pkg_cache_with_notice(&mut app);
  drop(lock_guard);

  match result {
    Ok((cmd, target)) => app.emit_success(cmd, &target),
    Err(err) => {
      app.emit_error(&err);
      process::exit(1);
    }
  }
}

fn run_command(app: &mut App, args: &[String]) -> CliResult<(&'static str, String)> {
  let command = args[0].as_str();
  match command {
    "-S" => {
      if let Some(selector) = args.get(1) {
        app.set_phase("install");
        need_cmd("opkg")?;
        install::app_auto(app, selector)?;
        Ok(("-S", selector.clone()))
      } else {
        app.set_phase("sync");
        sync::repos(app)?;
        Ok(("-S", "repos".to_string()))
      }
    }
    "-Su" => {
      app.set_phase("upgrade");
      need_cmd("opkg")?;
      upgrade::all(app)?;
      Ok(("-Su", "all".to_string()))
    }
    "-Syu" => {
      app.set_phase("sync");
      need_cmd("opkg")?;
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
      let pkgname = require_arg(args, 1, "-Qi requires <pkgname>")?;
      query::info(app, pkgname)?;
      Ok(("-Qi", pkgname.to_string()))
    }
    "-Ql" => {
      app.set_phase("query");
      let pkgname = require_arg(args, 1, "-Ql requires <pkgname>")?;
      query::files(app, pkgname)?;
      Ok(("-Ql", pkgname.to_string()))
    }
    "-So" => {
      app.set_phase("install");
      need_cmd("opkg")?;
      let selector = require_arg(args, 1, "-So requires <selector>")?;
      install::app_via_opkg(app, selector)?;
      Ok(("-So", selector.to_string()))
    }
    "-Si" => {
      app.set_phase("query");
      let selector = require_arg(args, 1, "-Si requires <selector>")?;
      query::sync_info(app, selector)?;
      Ok(("-Si", selector.to_string()))
    }
    "-Sp" => {
      app.set_phase("install");
      need_cmd("opkg")?;
      need_cmd("tar")?;
      let selector = require_arg(args, 1, "-Sp requires <selector>")?;
      install::pica_from_repo(app, selector)?;
      Ok(("-Sp", selector.to_string()))
    }
    "-U" => {
      app.set_phase("install");
      need_cmd("opkg")?;
      need_cmd("tar")?;
      let source = require_arg(args, 1, "-U requires <pkgfile|url>")?;
      install::pkg_source(app, source, None)?;
      Ok(("-U", source.to_string()))
    }
    "-R" => {
      app.set_phase("remove");
      need_cmd("opkg")?;
      let pkgname = require_arg(args, 1, "-R requires <pkgname>")?;
      remove::pkg(app, pkgname)?;
      Ok(("-R", pkgname.to_string()))
    }
    other => Err(CliError::new(E_ARG_INVALID, format!("unknown arg: {other}"))),
  }
}
