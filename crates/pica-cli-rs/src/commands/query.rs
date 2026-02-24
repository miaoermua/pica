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
    println!(
        "Pkgver          : {}",
        manifest_get_first(manifest, "pkgver")
    );
    println!(
        "Pkgrel          : {}",
        manifest_get_first(manifest, "pkgrel")
    );
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
    println!(
        "LuCI URL        : {}",
        manifest_get_first(manifest, "luci_url")
    );
    println!(
        "Version Tag     : {}",
        manifest_get_first(manifest, "version")
    );
    println!(
        "Branch          : {}",
        manifest_get_first(manifest, "branch")
    );
    println!(
        "Protocol        : {}",
        manifest_get_first(manifest, "protocol")
    );
    println!(
        "LuCI Desc       : {}",
        manifest_get_first(manifest, "luci_desc")
    );
    println!(
        "Platform        : {}",
        manifest_get_first(manifest, "platform")
    );
    println!("Arch            : {}", manifest_get_first(manifest, "arch"));
    println!(
        "Uname           : {}",
        manifest_get_first(manifest, "uname")
    );
    println!("Type            : {}", manifest_get_first(manifest, "type"));
    println!(
        "License         : {}",
        manifest_get_first(manifest, "license")
    );
    println!(
        "Proprietary     : {}",
        manifest_get_first(manifest, "proprietary")
    );

    Ok(())
}

pub fn query_license(app: &mut App, pkgname: &str) -> CliResult<()> {
    ensure_dirs(&app.paths)?;
    let db = read_json_file(&app.paths.db_file)?;

    let Some(manifest) = db
        .get("installed")
        .and_then(Value::as_object)
        .and_then(|installed| installed.get(pkgname))
        .and_then(|entry| entry.get("manifest"))
    else {
        return Err(CliError::new(
            E_ARG_INVALID,
            format!("not installed: {pkgname}"),
        ));
    };

    let license = manifest_get_first(manifest, "license");
    let proprietary = manifest_get_first(manifest, "proprietary");

    if !license.is_empty() {
        println!("{license}");
    }
    if !proprietary.is_empty() {
        println!("proprietary={proprietary}");
    }

    Ok(())
}
