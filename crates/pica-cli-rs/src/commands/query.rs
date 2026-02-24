use crate::{
    ensure_dirs, manifest_get_first, pkgver_cmp_key, App, CliError, CliResult, E_ARG_INVALID,
    E_DB_INVALID,
};
use crate::state::read_json_file;
use serde_json::Value;

pub fn query_installed(app: &mut App) -> CliResult<()> {
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
        let manifest = entry.get("manifest").unwrap_or(&Value::Null);
        let pkgver = manifest_get_first(manifest, "pkgver");
        let pkgrel = manifest_get_first(manifest, "pkgrel");
        let version = pkgver_cmp_key(&pkgver, &pkgrel);
        let platform = manifest_get_first(manifest, "platform");

        println!("{pkgname}\t{version}\t{platform}");
    }

    Ok(())
}

pub fn query_info(app: &mut App, pkgname: &str) -> CliResult<()> {
    ensure_dirs(&app.paths)?;
    let db = read_json_file(&app.paths.db_file)?;

    let Some(entry) = db
        .get("installed")
        .and_then(Value::as_object)
        .and_then(|installed| installed.get(pkgname))
    else {
        return Err(CliError::new(
            E_ARG_INVALID,
            format!("not installed: {pkgname}"),
        ));
    };

    let manifest = entry.get("manifest").unwrap_or(&Value::Null);

    println!("Name            : {pkgname}");
    println!(
        "Version         : {}",
        pkgver_cmp_key(
            &manifest_get_first(manifest, "pkgver"),
            &manifest_get_first(manifest, "pkgrel")
        )
    );
    println!("Pkgver          : {}", manifest_get_first(manifest, "pkgver"));
    println!("Pkgrel          : {}", manifest_get_first(manifest, "pkgrel"));
    println!(
        "AppName         : {}",
        manifest_get_first(manifest, "appname")
    );
    let program_url = {
        let value = manifest_get_first(manifest, "url");
        if value.is_empty() {
            manifest_get_first(manifest, "origin")
        } else {
            value
        }
    };
    println!("Program URL     : {}", program_url);
    println!("PkgDesc         : {}", manifest_get_first(manifest, "pkgdesc"));
    println!(
        "LuCI URL        : {}",
        manifest_get_first(manifest, "luci_url")
    );
    println!("Protocol        : {}", manifest_get_first(manifest, "protocol"));
    println!("Branch          : {}", manifest_get_first(manifest, "branch"));
    println!("LuCI Desc       : {}", manifest_get_first(manifest, "luci_desc"));
    println!("Platform        : {}", manifest_get_first(manifest, "platform"));
    println!("Arch            : {}", manifest_get_first(manifest, "arch"));
    println!("Uname           : {}", manifest_get_first(manifest, "uname"));
    println!("Type            : {}", manifest_get_first(manifest, "type"));
    println!("Source          : {}", manifest_get_first(manifest, "source"));
    println!("PkgMgr          : {}", manifest_get_first(manifest, "pkgmgr"));
    println!("Packager        : {}", manifest_get_first(manifest, "packager"));
    println!("Build Date      : {}", manifest_get_first(manifest, "builddate"));
    println!("Installed At    : {}", entry.get("installed_at").and_then(Value::as_u64).map(|value| value.to_string()).unwrap_or_default());
    println!("Installed From  : {}", entry.get("pkgfile").and_then(Value::as_str).unwrap_or(""));
    println!("License         : {}", manifest_get_first(manifest, "license"));
    println!("Proprietary     : {}", manifest_get_first(manifest, "proprietary"));

    Ok(())
}

pub fn query_files(app: &mut App, pkgname: &str) -> CliResult<()> {
    ensure_dirs(&app.paths)?;
    let db = read_json_file(&app.paths.db_file)?;

    let Some(entry) = db
        .get("installed")
        .and_then(Value::as_object)
        .and_then(|installed| installed.get(pkgname))
    else {
        return Err(CliError::new(
            E_ARG_INVALID,
            format!("not installed: {pkgname}"),
        ));
    };

    let files = entry
        .get("files")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    for item in files {
        if let Some(path) = item.as_str() {
            println!("{path}");
        }
    }

    Ok(())
}
