use crate::app::{
  App, CliError, CliResult, E_CONFIG_INVALID, E_IO, E_POLICY_INVALID, E_RUNTIME, FeedPolicy,
  prompt_yn,
};
use std::fs;
use std::path::Path;

/// Where an artifact's declared dependencies are satisfied from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DependencySource {
  /// The backend's feeds, by the names the manifest declares.
  Feeds,
  /// The containers the artifact ships in `pkgs/`.
  Containers,
}

/// Decides where the artifact's dependencies come from, once for the whole
/// request.
///
/// `declared` is every package name the artifact declares — `base` and `app`
/// together — and `have_pkgs_dir` says whether it shipped containers. One
/// decision for both phases, because `pkgs/` is a single set of containers owned
/// by the artifact rather than a per-phase resource: the packer filters it by
/// backend only, so it says nothing about which phase a container belongs to. A
/// policy asked once per phase would answer "the containers" twice, scheduling
/// the same batch twice, or answer "feeds" for a phase whose names only exist in
/// a container.
///
/// `Ok(None)` means there is nothing to install either way.
pub(crate) fn dependency_source(
  app: &App,
  declared: &[String],
  have_pkgs_dir: bool,
) -> CliResult<Option<DependencySource>> {
  if declared.is_empty() && !have_pkgs_dir {
    return Ok(None);
  }

  match app.options.feed_policy {
    // Feed-only refuses the artifact's own containers: an artifact that ships
    // them without declaring the packages they replace has nothing this policy
    // will install.
    FeedPolicy::FeedOnly => {
      if declared.is_empty() {
        return Err(CliError::new(
          E_POLICY_INVALID,
          "feed-only policy: the artifact declares no packages, and its packaged containers \
           may not be used (declare them, or install with another --feed-policy)"
            .to_string(),
        ));
      }
      Ok(Some(DependencySource::Feeds))
    }
    // The reverse: nothing but the containers may be used, so an artifact that
    // ships none has no source left.
    FeedPolicy::PackagedOnly => {
      if !have_pkgs_dir {
        return Err(CliError::new(
          E_RUNTIME,
          "packaged-only policy: the artifact ships no containers, and feeds may not be used"
            .to_string(),
        ));
      }
      Ok(Some(DependencySource::Containers))
    }
    // Feeds win when they have anything to offer; containers are the fallback.
    FeedPolicy::FeedFirst => Ok(Some(if declared.is_empty() {
      DependencySource::Containers
    } else {
      DependencySource::Feeds
    })),
    // The artifact's own containers win; feeds are the fallback.
    FeedPolicy::PackagedFirst => {
      Ok(Some(if have_pkgs_dir { DependencySource::Containers } else { DependencySource::Feeds }))
    }
    FeedPolicy::Ask => {
      // Without containers the declarations are the only source there is, and
      // asking would offer a choice that does not exist.
      if !have_pkgs_dir {
        return Ok(Some(DependencySource::Feeds));
      }
      if declared.is_empty() {
        return Ok(Some(DependencySource::Containers));
      }

      // No backend at all means no feeds at all, which is an answer rather than
      // a failure: the containers are the source that is left.
      let Ok(package_manager) = app.package_manager() else {
        return Ok(Some(DependencySource::Containers));
      };
      // An index that cannot be made readable stops the question here: counting
      // the declared packages against a feed nobody could fetch would report
      // "0/n in the feeds" for a host whose feeds were merely unreachable.
      //
      // No downgrade for optional-only artifacts: `declared` is the required
      // list, and an empty one returned above, so this prompt only exists when
      // there is something the index has to answer for.
      package_manager.update_index()?;
      // An empty entry is not a package anybody could look up, so it counts on
      // neither side of the fraction.
      let named: Vec<&String> = declared.iter().filter(|dep| !dep.is_empty()).collect();
      let total = named.len();
      let mut available = 0;
      for dep in &named {
        if package_manager.package_exists(dep)? {
          available += 1;
        }
      }

      // Nothing the artifact declares is in the feeds, so the containers are the
      // only way to satisfy it.
      if available == 0 {
        return Ok(Some(DependencySource::Containers));
      }
      if app.options.non_interactive {
        return Ok(Some(DependencySource::Feeds));
      }

      let use_feeds = prompt_yn(
        &format!(
          "Found {available}/{total} declared packages in the {} feeds. Use feeds instead of the \
           packaged containers?",
          app.backend().as_str()
        ),
        true,
      );
      Ok(Some(if use_feeds { DependencySource::Feeds } else { DependencySource::Containers }))
    }
  }
}

/// Installs every native package shipped in the artifact's `pkgs/`, in one call
/// to the backend. Only the artifact's own backend packages are present, because
/// the packer filters by backend, so no extension-based filtering happens here.
///
/// One call, and not one per container: the containers of a single artifact
/// depend on one another — an app container and the core container it needs
/// arrive together — and a backend resolves such a dependency only among the
/// files of the same request. Handing them over one at a time would leave the
/// order of the loop, alphabetical or otherwise, deciding whether the install
/// works, and no order pica can pick is a topological one.
///
/// `label` names the group in messages. There is deliberately no per-phase
/// split here: nothing in the artifact says which container serves `base` and
/// which serves `app`, so any split would be a guess — and a guess that
/// separates two dependent containers is worse than one batch.
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

  let targets: Vec<String> = paths.iter().map(|path| path.display().to_string()).collect();
  app.package_manager()?.install(label, &targets)?;

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
  use super::{
    DependencySource, dependency_source, install_pkgs_dir, pkg_list_diff_added, reorder_app_list,
  };
  use crate::app::{App, FeedPolicy, JsonMode, OptionalDeps, Options, Paths};
  use pretty_assertions::assert_eq;

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

  fn app_with(policy: FeedPolicy) -> App {
    App::new(
      Paths::from_env(),
      Options {
        json_mode: JsonMode::None,
        non_interactive: true,
        feed_policy: policy,
        fetch_timeout: 30,
        fetch_retry: 2,
        fetch_retry_delay: 1,
        pkgmgr: None,
        optional_deps: OptionalDeps::None,
      },
    )
  }

  fn declared() -> Vec<String> {
    vec!["base-a".to_string(), "app-b".to_string()]
  }

  #[test]
  fn the_source_is_decided_once_for_the_whole_request() {
    // `pkgs/` belongs to the artifact, not to a phase, so the answer is about the
    // artifact: asking per phase is what let the same batch be scheduled twice.
    let cases = [
      (FeedPolicy::FeedOnly, true, Some(DependencySource::Feeds)),
      (FeedPolicy::PackagedOnly, true, Some(DependencySource::Containers)),
      (FeedPolicy::FeedFirst, true, Some(DependencySource::Feeds)),
      (FeedPolicy::PackagedFirst, true, Some(DependencySource::Containers)),
    ];
    for (policy, have_pkgs_dir, expected) in cases {
      let app = app_with(policy);
      let source = dependency_source(&app, &declared(), have_pkgs_dir).expect("decided");
      assert_eq!(source, expected, "{policy:?}");
    }

    // Nothing declared and no containers: no source, and no error either.
    let app = app_with(FeedPolicy::Ask);
    assert_eq!(dependency_source(&app, &[], false).expect("decided"), None);

    // Containers are the only source when the declarations are missing, which is
    // the case an artifact with a packaged app and no manifest list lands in.
    let app = app_with(FeedPolicy::FeedFirst);
    assert_eq!(
      dependency_source(&app, &[], true).expect("decided"),
      Some(DependencySource::Containers),
      "a name-less artifact falls back to the batch rather than installing nothing"
    );
    let app = app_with(FeedPolicy::Ask);
    assert_eq!(
      dependency_source(&app, &[], true).expect("decided"),
      Some(DependencySource::Containers)
    );

    // And feeds are the only source when no containers were shipped at all.
    let app = app_with(FeedPolicy::PackagedFirst);
    assert_eq!(
      dependency_source(&app, &declared(), false).expect("decided"),
      Some(DependencySource::Feeds)
    );
  }

  #[test]
  fn a_policy_without_a_source_refuses_instead_of_installing_nothing() {
    // Feed-only will not touch the containers, and the declarations it needs are
    // absent: the install has to stop rather than quietly do nothing.
    let err = dependency_source(&app_with(FeedPolicy::FeedOnly), &[], true)
      .expect_err("feed-only has no packages to install");
    assert!(err.message.contains("feed-only"), "{}", err.message);

    // The reverse: packaged-only has no containers to install.
    let err = dependency_source(&app_with(FeedPolicy::PackagedOnly), &declared(), false)
      .expect_err("packaged-only has nothing to install");
    assert!(err.message.contains("packaged-only"), "{}", err.message);
  }

  #[test]
  fn install_pkgs_dir_rejects_empty_directory() {
    let app = app_with(FeedPolicy::PackagedOnly);
    let dir = tempfile::tempdir().expect("create temp dir");
    let err = install_pkgs_dir(&app, "packaged", dir.path()).expect_err("empty pkgs dir must fail");
    assert!(err.message.contains("no packages found"), "{}", err.message);
  }
}
