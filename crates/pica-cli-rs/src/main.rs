mod lock;
mod commands;
mod types;
mod state;
mod system;
mod helpers;

use crate::types::{
    ensure_dirs, parse_options, require_arg, App, CliError, CliResult, FeedPolicy, JsonMode,
    Options, Paths, DEFAULT_ERROR_CODE,
};
use crate::state::read_json_file;
use crate::helpers::*;
use pica_core::manifest::{get_first as manifest_get_first, Manifest};
use pica_core::repo::is_supported_url;
use pica_core::selector::Selector;
use pica_core::version::{pkgver_cmp_key, pkgver_ge, ver_ge};
use pica_core::PICA_VERSION;
use std::env;
use std::process::{self};
use crate::lock::LockGuard;
use crate::commands::query::{query_info, query_installed, query_license};
use crate::commands::remove::remove_pkg;
use crate::commands::sync::sync_repos;
use crate::commands::upgrade::upgrade_all;
use crate::commands::install::{
    install_app_auto, install_app_via_opkg, install_pica_from_repo, install_pkg_source,
};

fn usage() {
    println!(
        "Usage:\n  pica-rs -S                 Sync (download repo.json and update index)\n  pica-rs -Su                Upgrade all installed pica packages\n  pica-rs -Syu               Sync, then upgrade all installed pica packages\n  pica-rs -Si <selector>     Install by selector (auto: opkg if available, else pica)\n  pica-rs -So <selector>     Install by selector (force opkg)\n  pica-rs -Sp <selector>     Install by selector (force pica repo)\n  pica-rs -U <pkgfile|url>   Install/Update from local file or URL\n  pica-rs -R <pkgname>       Remove package (no dependency handling)\n  pica-rs -Q                 List installed pica packages\n  pica-rs -Qi <pkgname>      Show installed package info\n  pica-rs -Ql <pkgname>      Show installed package license\n  pica-rs --json ...         Emit JSON on success and error (explicit only)\n  pica-rs --json-errors ...  Emit JSON only on error\n  pica-rs --non-interactive ...\n                            Disable prompts (for backend/automation)\n  pica-rs --feed-policy <mode>\n                            ask|feed-first|packaged-first|feed-only|packaged-only\n  pica-rs --version\n\nNotes:\n  - Requires: opkg, tar, and one fetcher (uclient-fetch/wget/curl) for URL install/sync.\n  - Config: /etc/pica/pica.json\n  - State:  /var/lib/pica/db.json, /var/lib/pica/index.json\n  - Lock:   /var/lib/pica/db.lck\n  - Selector example: app:version(branch)"
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
    if command == "--version" {
        println!("{PICA_VERSION}");
        app.emit_success("--version", PICA_VERSION);
        return;
    }

    if app.options.json_mode != JsonMode::None && !has_command("jq") {
        let err = CliError::new(
            "E_MISSING_COMMAND",
            "--json/--json-errors requires command: jq",
        );
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
            app.set_phase("sync");
            sync_repos(app)?;
            Ok(("-S", "repos".to_string()))
        }
        "-Su" => {
            app.set_phase("upgrade");
            need_cmd("opkg")?;
            upgrade_all(app)?;
            Ok(("-Su", "all".to_string()))
        }
        "-Syu" => {
            app.set_phase("sync");
            need_cmd("opkg")?;
            sync_repos(app)?;
            app.set_phase("upgrade");
            upgrade_all(app)?;
            Ok(("-Syu", "all".to_string()))
        }
        "-Q" => {
            app.set_phase("query");
            query_installed(app)?;
            Ok(("-Q", "installed".to_string()))
        }
        "-Qi" => {
            app.set_phase("query");
            let pkgname = require_arg(args, 1, "-Qi requires <pkgname>")?;
            query_info(app, pkgname)?;
            Ok(("-Qi", pkgname.to_string()))
        }
        "-Ql" => {
            app.set_phase("query");
            let pkgname = require_arg(args, 1, "-Ql requires <pkgname>")?;
            query_license(app, pkgname)?;
            Ok(("-Ql", pkgname.to_string()))
        }
        "-So" => {
            app.set_phase("install");
            need_cmd("opkg")?;
            let selector = require_arg(args, 1, "-So requires <selector>")?;
            install_app_via_opkg(app, selector)?;
            Ok(("-So", selector.to_string()))
        }
        "-Si" => {
            app.set_phase("install");
            need_cmd("opkg")?;
            let selector = require_arg(args, 1, "-Si requires <selector>")?;
            install_app_auto(app, selector)?;
            Ok(("-Si", selector.to_string()))
        }
        "-Sp" => {
            app.set_phase("install");
            need_cmd("opkg")?;
            need_cmd("tar")?;
            let selector = require_arg(args, 1, "-Sp requires <selector>")?;
            install_pica_from_repo(app, selector)?;
            Ok(("-Sp", selector.to_string()))
        }
        "-U" => {
            app.set_phase("install");
            need_cmd("opkg")?;
            need_cmd("tar")?;
            let source = require_arg(args, 1, "-U requires <pkgfile|url>")?;
            install_pkg_source(app, source, None)?;
            Ok(("-U", source.to_string()))
        }
        "-R" => {
            app.set_phase("remove");
            need_cmd("opkg")?;
            let pkgname = require_arg(args, 1, "-R requires <pkgname>")?;
            remove_pkg(app, pkgname)?;
            Ok(("-R", pkgname.to_string()))
        }
        other => Err(CliError::new(
            DEFAULT_ERROR_CODE,
            format!("unknown arg: {other}"),
        )),
    }
}
