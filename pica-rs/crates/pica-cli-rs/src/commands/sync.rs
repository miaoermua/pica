use crate::{
    detect_platform, ensure_dirs, ensure_json_object_field, fetch_url, has_command, opkg_has_package,
    opkg_update_ignore, read_json_file, write_json_atomic_pretty, App, CliError, CliResult,
    DEFAULT_ERROR_CODE,
};
use pica_core::repo::parse_repo_json;
use serde_json::{json, Value};

pub fn sync_repos(app: &mut App) -> CliResult<()> {
    ensure_dirs(&app.paths)?;
    app.log_info("Synchronizing package databases...");

    let conf = read_json_file(&app.paths.conf_file)?;
    let repos = conf
        .get("repos")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    if repos.is_empty() {
        return Err(CliError::new(
            DEFAULT_ERROR_CODE,
            format!("no repos configured in {}", app.paths.conf_file.display()),
        ));
    }

    let host_platform = detect_platform();
    let now = now_unix_secs();

    let mut index = read_json_file(&app.paths.index_file)?;
    if !index.is_object() {
        index = json!({"schema": 1, "repos": {}});
    }
    index["schema"] = json!(1);
    ensure_json_object_field(&mut index, "repos")?;

    for (repo_index, repo_entry) in repos.iter().enumerate() {
        let name = repo_entry
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let url = repo_entry
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let repo_platform = repo_entry
            .get("platform")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();

        if name.is_empty() {
            return Err(CliError::new(
                "E_CONFIG_INVALID",
                format!("repo[{repo_index}] missing name"),
            ));
        }
        if url.is_empty() {
            return Err(CliError::new(
                "E_CONFIG_INVALID",
                format!("repo[{repo_index}] missing url"),
            ));
        }

        app.log_info(format!("{name} downloading..."));
        let repo_json_url = format!("{}/repo.json", url.trim_end_matches('/'));
        let repo_raw = fetch_url(&repo_json_url)?;
        let repo_text = String::from_utf8(repo_raw).map_err(|_| {
            CliError::new(
                "E_REPO_INVALID",
                format!("{name}: repo.json is not valid UTF-8"),
            )
        })?;
        let parsed_repo = parse_repo_json(&repo_text).map_err(|error| {
            CliError::new(
                "E_REPO_INVALID",
                format!("{name}: repo.json failed strict schema/filename validation: {error}"),
            )
        })?;

        let repo_value = serde_json::to_value(&parsed_repo)
            .map_err(|error| CliError::new("E_REPO_INVALID", error.to_string()))?;

        let repo_cache_file = app.paths.repos_cache_dir.join(format!("{name}.json"));
        write_json_atomic_pretty(&repo_cache_file, &repo_value)?;

        let effective_platform = if repo_platform.is_empty() {
            host_platform.clone()
        } else {
            repo_platform.to_string()
        };

        let repo_obj = json!({
            "name": name,
            "url": url,
            "updated_at": now,
            "platform": effective_platform,
            "data": repo_value,
        });

        let repos_obj = index
            .get_mut("repos")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| CliError::new("E_INDEX_INVALID", "index repos is not object"))?;
        repos_obj.insert(name.to_string(), repo_obj);

        app.log_info(format!("{name} updated"));
    }

    write_json_atomic_pretty(&app.paths.index_file, &index)?;

    check_index_dependencies(app);
    Ok(())
}

fn check_index_dependencies(app: &mut App) {
    if !has_command("opkg") {
        return;
    }

    opkg_update_ignore();

    let Ok(index) = read_json_file(&app.paths.index_file) else {
        return;
    };

    let mut missing_any = false;
    for dep in collect_declared_dependencies(&index) {
        if !opkg_has_package(&dep) {
            app.log_warn(format!("opkg feed missing dependency: {dep}"));
            missing_any = true;
        }
    }

    if missing_any {
        app.log_warn("Some declared dependencies are missing in opkg feeds");
    }
}

fn collect_declared_dependencies(index: &Value) -> Vec<String> {
    let mut values = Vec::new();

    let Some(repos) = index.get("repos").and_then(Value::as_object) else {
        return values;
    };

    for repo in repos.values() {
        let Some(packages) = repo
            .get("data")
            .and_then(|data| data.get("packages"))
            .and_then(Value::as_array)
        else {
            continue;
        };

        for package in packages {
            let manifest = package.get("manifest").unwrap_or(&Value::Null);
            append_dep_list(&mut values, manifest.get("base"));
            append_dep_list(&mut values, manifest.get("kmod"));
            append_dep_list(&mut values, manifest.get("app"));
        }
    }

    values.sort();
    values.dedup();
    values
}

fn append_dep_list(output: &mut Vec<String>, value: Option<&Value>) {
    match value {
        Some(Value::Array(values)) => {
            for entry in values {
                if let Some(text) = entry.as_str() {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        output.push(trimmed.to_string());
                    }
                }
            }
        }
        Some(Value::String(text)) => {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                output.push(trimmed.to_string());
            }
        }
        _ => {}
    }
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}
