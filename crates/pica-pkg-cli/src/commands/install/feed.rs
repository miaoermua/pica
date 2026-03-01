use crate::app::{
  prompt_yn, App, CliError, CliResult, FeedPolicy, E_CONFIG_INVALID, E_IO, E_POLICY_INVALID,
  E_RUNTIME,
};
use crate::system::{opkg_has_package, opkg_install_pkg, opkg_update_ignore};
use std::fs;
use std::path::Path;

pub(crate) fn should_use_feeds(
  app: &App,
  label: &str,
  pkg_list: &[String],
  have_ipk_dir: bool,
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
    FeedPolicy::PackagedFirst => i8::from(!have_ipk_dir),
    FeedPolicy::Ask => {
      if !have_ipk_dir {
        return 1;
      }
      if pkg_list.is_empty() {
        return 0;
      }

      opkg_update_ignore();
      let mut total = 0usize;
      let mut available = 0usize;
      for dep in pkg_list {
        if dep.is_empty() {
          continue;
        }
        total += 1;
        if opkg_has_package(dep) {
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
          "Found {available}/{total} {label} packages in opkg feeds. Use feeds instead of packaged ipks?"
        ),
        true,
      ))
    }
  }
}

pub(crate) fn install_via_feeds_or_ipk(
  app: &App,
  label: &str,
  pkg_list: &[String],
  ipk_dir: &Path,
  have_ipk_dir: bool,
) -> CliResult<()> {
  if pkg_list.is_empty() && !have_ipk_dir {
    return Ok(());
  }

  let use_feeds = should_use_feeds(app, label, pkg_list, have_ipk_dir);
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
      opkg_install_pkg(label, dep)?;
    }
    return Ok(());
  }

  if have_ipk_dir {
    install_ipk_dir(label, ipk_dir)?;
    return Ok(());
  }

  Err(CliError::new(
    E_RUNTIME,
    format!("{label} not available in feeds and no packaged ipks provided"),
  ))
}

pub(crate) fn install_ipk_dir(label: &str, dir: &Path) -> CliResult<()> {
  if !dir.is_dir() {
    return Ok(());
  }

  let mut installed_any = false;
  let entries = fs::read_dir(dir)
    .map_err(|err| CliError::new(E_IO, format!("read {} failed: {err}", dir.display())))?;

  for entry in entries.flatten() {
    let path = entry.path();
    if path.extension().and_then(|v| v.to_str()) != Some("ipk") {
      continue;
    }
    opkg_install_pkg(label, &path.display().to_string())?;
    installed_any = true;
  }

  if !installed_any {
    return Err(CliError::new(
      E_CONFIG_INVALID,
      format!("no ipk files found in {label} dir: {}", dir.display()),
    ));
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
  use super::{install_via_feeds_or_ipk, pkg_list_diff_added, reorder_app_list, should_use_feeds};
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
  fn install_via_feeds_or_ipk_skips_empty_optional_group() {
    let app = App::new(
      Paths::from_env(),
      Options {
        json_mode: JsonMode::None,
        non_interactive: true,
        feed_policy: FeedPolicy::Ask,
        fetch_timeout: 30,
        fetch_retry: 2,
        fetch_retry_delay: 1,
      },
    );

    let result = install_via_feeds_or_ipk(&app, "base", &[], Path::new("/nonexistent"), false);
    assert!(result.is_ok());
  }
}
