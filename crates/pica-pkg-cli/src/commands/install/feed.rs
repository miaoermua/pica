use crate::app::{
  prompt_yn, App, CliError, CliResult, FeedPolicy, E_CONFIG_INVALID, E_IO, E_POLICY_INVALID,
  E_RUNTIME,
};
use std::fs;
use std::path::Path;

pub(crate) fn should_use_feeds(
  app: &App,
  label: &str,
  pkg_list: &[String],
  have_pkgs_dir: bool,
) -> i8 {
  match app.options.feed_policy {
    FeedPolicy::FeedOnly => {
      if pkg_list.is_empty() {
        -1
      } else {
        1
      }
    }
    FeedPolicy::PackagedOnly => 0,
    FeedPolicy::FeedFirst => i8::from(!pkg_list.is_empty()),
    FeedPolicy::PackagedFirst => i8::from(!have_pkgs_dir),
    FeedPolicy::Ask => {
      if !have_pkgs_dir {
        return 1;
      }
      if pkg_list.is_empty() {
        return 0;
      }

      if let Ok(package_manager) = app.package_manager() {
        package_manager.update_index();
      }
      let mut total = 0usize;
      let mut available = 0usize;
      for dep in pkg_list {
        if dep.is_empty() {
          continue;
        }
        total += 1;
        if app.package_manager().is_ok_and(|manager| manager.package_exists(dep)) {
          available += 1;
        }
      }

      if available == 0 {
        return 0;
      }

      if app.options.non_interactive {
        return 1;
      }

      i8::from(prompt_yn(
        &format!(
          "Found {available}/{total} {label} packages in the {} feeds. Use feeds instead of packaged packages?",
          app.backend().as_str()
        ),
        true,
      ))
    }
  }
}

pub(crate) fn install_via_feeds_or_pkgs(
  app: &App,
  label: &str,
  pkg_list: &[String],
  pkgs_dir: &Path,
  have_pkgs_dir: bool,
) -> CliResult<()> {
  if pkg_list.is_empty() && !have_pkgs_dir {
    return Ok(());
  }

  let use_feeds = should_use_feeds(app, label, pkg_list, have_pkgs_dir);
  if use_feeds == -1 {
    return Err(CliError::new(
      E_POLICY_INVALID,
      format!("{label} requires feed packages under feed-only policy"),
    ));
  }

  if use_feeds == 1 {
    if pkg_list.is_empty() {
      return Err(CliError::new(
        E_CONFIG_INVALID,
        format!("{label} packages not defined in manifest"),
      ));
    }
    for dep in pkg_list {
      if dep.is_empty() {
        continue;
      }
      app.package_manager()?.install(label, dep)?;
    }
    return Ok(());
  }

  if have_pkgs_dir {
    install_pkgs_dir(app, label, pkgs_dir)?;
    return Ok(());
  }

  Err(CliError::new(
    E_RUNTIME,
    format!("{label} not available in feeds and no packaged packages provided"),
  ))
}

/// Installs every native package shipped in the artifact's `pkgs/`. Only the
/// artifact's own backend packages are present, because the packer filters by
/// backend, so no extension-based filtering happens here.
pub(crate) fn install_pkgs_dir(app: &App, label: &str, dir: &Path) -> CliResult<()> {
  if !dir.is_dir() {
    return Ok(());
  }

  let mut paths: Vec<std::path::PathBuf> = fs::read_dir(dir)
    .map_err(|err| CliError::new(E_IO, format!("read {} failed: {err}", dir.display())))?
    .flatten()
    .map(|entry| entry.path())
    .filter(|path| path.is_file())
    .collect();
  paths.sort();

  if paths.is_empty() {
    return Err(CliError::new(
      E_CONFIG_INVALID,
      format!("no packages found in {label} dir: {}", dir.display()),
    ));
  }

  for path in paths {
    app.package_manager()?.install(label, &path.display().to_string())?;
  }

  Ok(())
}

pub(crate) fn reorder_app_list(list: Vec<String>) -> Vec<String> {
  let mut core = Vec::new();
  let mut luci = Vec::new();
  let mut i18n = Vec::new();

  for item in list {
    if item.is_empty() {
      continue;
    }
    if item.starts_with("luci-i18n-") {
      i18n.push(item);
    } else if item.starts_with("luci-app-") {
      luci.push(item);
    } else {
      core.push(item);
    }
  }

  core.extend(luci);
  core.extend(i18n);
  core
}

pub(crate) fn pkg_list_diff_added(before: &[String], after: &[String]) -> Vec<String> {
  let before_set: std::collections::HashSet<&str> = before.iter().map(String::as_str).collect();
  let mut out = Vec::new();
  for item in after {
    if !before_set.contains(item.as_str()) {
      out.push(item.clone());
    }
  }
  out
}

#[cfg(test)]
mod tests {
  use super::{install_via_feeds_or_pkgs, pkg_list_diff_added, reorder_app_list, should_use_feeds};
  use crate::app::{App, FeedPolicy, JsonMode, Options, Paths};
  use pretty_assertions::assert_eq;
  use std::path::Path;

  #[test]
  fn reorder_app_list_moves_luci_and_i18n_last() {
    let input = vec![
      "luci-i18n-foo-zh-cn".to_string(),
      "foo-core".to_string(),
      "luci-app-foo".to_string(),
      "foo-helper".to_string(),
    ];

    let output = reorder_app_list(input);
    assert_eq!(
      output,
      vec![
        "foo-core".to_string(),
        "foo-helper".to_string(),
        "luci-app-foo".to_string(),
        "luci-i18n-foo-zh-cn".to_string(),
      ]
    );
  }

  #[test]
  fn pkg_list_diff_added_returns_only_new_items() {
    let before = vec!["a".to_string(), "b".to_string()];
    let after = vec!["a".to_string(), "b".to_string(), "c".to_string()];

    let added = pkg_list_diff_added(&before, &after);
    assert_eq!(added, vec!["c".to_string()]);
  }

  #[test]
  fn should_use_feeds_policy_matrix_basics() {
    let paths = Paths::from_env();
    let base = App::new(
      paths,
      Options {
        json_mode: JsonMode::None,
        non_interactive: true,
        feed_policy: FeedPolicy::FeedOnly,
        fetch_timeout: 30,
        fetch_retry: 2,
        fetch_retry_delay: 1,
        pkgmgr: None,
      },
    );

    let decision = should_use_feeds(&base, "app", &["a".to_string()], true);
    assert_eq!(decision, 1);

    let no_feed = should_use_feeds(&base, "app", &[], true);
    assert_eq!(no_feed, -1);

    let mut packaged_only = base;
    packaged_only.options.feed_policy = FeedPolicy::PackagedOnly;
    let packaged = should_use_feeds(&packaged_only, "app", &["a".to_string()], true);
    assert_eq!(packaged, 0);

    let mut feed_first = packaged_only;
    feed_first.options.feed_policy = FeedPolicy::FeedFirst;
    let feed_first_choice = should_use_feeds(&feed_first, "app", &["a".to_string()], true);
    assert_eq!(feed_first_choice, 1);

    let mut packaged_first = feed_first;
    packaged_first.options.feed_policy = FeedPolicy::PackagedFirst;
    let packaged_first_choice = should_use_feeds(&packaged_first, "app", &["a".to_string()], true);
    assert_eq!(packaged_first_choice, 0);
  }

  #[test]
  fn install_via_feeds_or_pkgs_skips_empty_optional_group() {
    let app = App::new(
      Paths::from_env(),
      Options {
        json_mode: JsonMode::None,
        non_interactive: true,
        feed_policy: FeedPolicy::Ask,
        fetch_timeout: 30,
        fetch_retry: 2,
        fetch_retry_delay: 1,
        pkgmgr: None,
      },
    );

    let result = install_via_feeds_or_pkgs(&app, "base", &[], Path::new("/nonexistent"), false);
    assert!(result.is_ok());
  }

  #[test]
  fn install_pkgs_dir_rejects_empty_directory() {
    let app = App::new(
      Paths::from_env(),
      Options {
        json_mode: JsonMode::None,
        non_interactive: true,
        feed_policy: FeedPolicy::PackagedOnly,
        fetch_timeout: 30,
        fetch_retry: 2,
        fetch_retry_delay: 1,
        pkgmgr: None,
      },
    );
    let dir = tempfile::tempdir().expect("create temp dir");
    let err = install_via_feeds_or_pkgs(&app, "app", &[], dir.path(), true)
      .expect_err("empty pkgs dir must fail");
    assert!(err.message.contains("no packages found"));
  }
}
