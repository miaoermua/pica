use crate::{
    ensure_dirs, find_pica_candidates_in_index, manifest_get_first, pkgver_cmp_key, pkgver_ge,
    App, CliError, CliResult, E_ARG_INVALID, E_CONFIG_INVALID, E_DB_INVALID,
};
use crate::state::read_json_file;
use serde_json::Value;

fn format_size_display(size_text: &str) -> String {
    if size_text.is_empty() {
        return String::new();
    }

    let Ok(size_bytes) = size_text.parse::<u64>() else {
        return size_text.to_string();
    };

    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

    let human = if (size_bytes as f64) >= GIB {
        format!("{:.2} GiB", (size_bytes as f64) / GIB)
    } else if (size_bytes as f64) >= MIB {
        format!("{:.2} MiB", (size_bytes as f64) / MIB)
    } else if (size_bytes as f64) >= KIB {
        format!("{:.2} KiB", (size_bytes as f64) / KIB)
    } else {
        format!("{} B", size_bytes)
    };

    format!("{} ({human})", size_bytes)
}

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

pub fn query_sync_info(app: &mut App, selector: &str) -> CliResult<()> {
    ensure_dirs(&app.paths)?;

    let candidates = find_pica_candidates_in_index(app, selector)?;
    if candidates.is_empty() {
        return Err(CliError::new(
            E_CONFIG_INVALID,
            format!("package not found in pica repos: {selector}"),
        ));
    }

    let mut best = &candidates[0];
    for candidate in &candidates[1..] {
        if pkgver_ge(&candidate.cmpver, &best.cmpver) {
            best = candidate;
        }
    }

    let download_url = if let Some(url) = &best.download_url {
        url.clone()
    } else {
        format!("{}/packages/{}", best.url.trim_end_matches('/'), best.filename)
    };

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
    println!("Download URL    : {}", download_url);
    println!("Filename        : {}", best.filename);
    println!("Sha256          : {}", best.sha256);
    println!(
        "Size            : {}",
        best.size
            .map(|value| format_size_display(&value.to_string()))
            .unwrap_or_default()
    );

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
    println!("OS              : {}", manifest_get_first(manifest, "os"));
    println!("Platform        : {}", manifest_get_first(manifest, "platform"));
    println!("Arch            : {}", manifest_get_first(manifest, "arch"));
    println!("Uname           : {}", manifest_get_first(manifest, "uname"));
    println!("Type            : {}", manifest_get_first(manifest, "type"));
    println!("Source          : {}", manifest_get_first(manifest, "source"));
    println!("PkgMgr          : {}", manifest_get_first(manifest, "pkgmgr"));
    println!("Packager        : {}", manifest_get_first(manifest, "packager"));
    println!("Build Date      : {}", manifest_get_first(manifest, "builddate"));
    println!(
        "Size            : {}",
        format_size_display(&manifest_get_first(manifest, "size"))
    );
    println!("Installed At    : {}", entry.get("installed_at").and_then(Value::as_u64).map(|value| value.to_string()).unwrap_or_default());
    println!("Installed From  : {}", entry.get("pkgfile").and_then(Value::as_str).unwrap_or(""));
    println!("License         : {}", manifest_get_first(manifest, "license"));
    println!("Visibility      : {}", manifest_get_first(manifest, "visibility"));

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

#[cfg(test)]
mod tests {
    use super::format_size_display;

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
