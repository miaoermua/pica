//! Transaction records, headers and summaries.

use super::*;
use pretty_assertions::assert_eq;

#[test]
fn an_unreadable_installed_list_keeps_what_the_transaction_already_recorded() {
  let installed = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
  let reads = std::rc::Rc::new(std::cell::RefCell::new(0));
  let stub = ListingStub {
    installed,
    reads,
    // The read after `base` answers; the one after `app` does not.
    fail_from_call: 1,
    fail_install_label: "app",
  };

  let work = tempfile::tempdir().expect("temp dir");
  let mut app = test_app(work.path());
  app.use_manager(Box::new(stub));

  let mut tx = InstallTransaction::new();
  tx.tx_before = Some(Vec::new());

  install_phase(&mut app, &mut tx, "base", &["base-dep".to_string()]).expect("base installs");
  assert_eq!(tx.tx_added, vec!["base-dep".to_string()]);

  install_phase(&mut app, &mut tx, "app", &["app-dep".to_string()])
    .expect_err("the app install fails");

  assert_eq!(
    tx.tx_added,
    vec!["base-dep".to_string()],
    "a listing that stopped answering must not erase what was already confirmed"
  );
  assert!(tx.tx_changed.is_empty(), "and it must not invent version changes");
}

#[test]
fn a_failed_walk_never_claims_a_path_that_was_already_there() {
  let mut fixture = PicaFixture::new();

  // A directory that existed before this install and that the payload also
  // writes into. The failed walk reports nothing, so the rollback works from
  // the paths this step could have created — and a path that was already
  // there is not one of them: removing it would delete what this install
  // never made.
  let pre_existing = fixture.path("keep");
  fs::create_dir_all(&pre_existing).expect("mkdir keep");

  let err = fixture
    .install_without_containers(
      &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
      &[("keep/file", "copied before the walk failed\n")],
      &[("zzz-escape", "/etc/passwd")],
      &[],
    )
    .expect_err("step 7 fails");

  assert!(err.message.contains("escapes the target root"), "{}", err.message);
  assert!(pre_existing.is_dir(), "a directory the install did not create must survive");
  assert!(!fixture.path("keep/file").exists(), "what the walk did copy is still taken back out");
  assert!(!fixture.path("usr/lib/demo/helper").exists(), "the container is rolled back too");
  assert!(
    db_record(&fixture.app, "demo").is_none(),
    "nothing was left behind, so there is nothing to record"
  );
}

#[test]
fn a_rollback_that_cannot_finish_keeps_a_record_to_remove_by_hand() {
  let mut fixture = PicaFixture::new();

  // The install hook runs, so the rollback knows a hook changed something only
  // this package can undo and tries to compensate with the package's own
  // `remove` hook. That hook is declared but fails, so the compensation cannot
  // finish — the case a surviving record exists for, because `-R` driven by
  // that record is the only thing left that can clean up.
  let install_hook = "#!/bin/sh\nmkdir -p /tmp/pica-install-ran\n";
  let failing_remove = "#!/bin/sh\nexit 1\n";

  let err = fixture
    .install_without_containers(
      &format!(
        "{MANIFEST_DEMO}pkgmgr = pica\nscript_install = scripts/install\nscript_remove = scripts/remove\n"
      ),
      // `aaa.conf` sorts ahead of the escaping symlink, so the walk copies it
      // and then trips, which is what starts the rollback in the first place.
      &[("aaa.conf", "copied before the walk failed\n")],
      &[("zzz-escape", "/etc/passwd")],
      &[("scripts/install", install_hook), ("scripts/remove", failing_remove)],
    )
    .expect_err("the payload walk trips on the escaping symlink");

  assert!(err.message.contains("rollback incomplete"), "{}", err.message);
  assert!(!fixture.path("aaa.conf").exists(), "what the walk did copy is taken back out");

  let record = db_record(&fixture.app, "demo")
    .expect("an incomplete rollback keeps a record, because -R is the only way left");
  assert!(
    record["files"].as_array().is_some_and(Vec::is_empty),
    "the recorded files were all removed; only the hook residue is left: {record}"
  );
}

#[test]
fn a_pica_artifact_without_containers_only_lays_the_overlay() {
  let mut fixture = PicaFixture::new();

  install_artifact(
    &fixture.work_path,
    &mut fixture.app,
    &fixture.target,
    &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
    &[("etc/demo.conf", "overlay\n")],
    &[],
    &[],
    &[],
  )
  .expect("install");

  assert_eq!(fs::read_to_string(fixture.path("etc/demo.conf")).expect("overlay"), "overlay\n");
  let record = db_record(&fixture.app, "demo").expect("recorded");
  assert_eq!(
    record["files"].as_array().map(Vec::len),
    Some(1),
    "the record holds the file the artifact deployed: {record}"
  );
}

#[test]
fn the_header_names_which_of_the_four_things_this_is() {
  assert_eq!(transaction_header("demo", "1.0.0-1", None), "Installing demo (1.0.0-1)");
  assert_eq!(
    transaction_header("demo", "1.1.0-1", Some("1.0.0-1")),
    "Upgrading demo (1.0.0-1 -> 1.1.0-1)"
  );
  assert_eq!(
    transaction_header("demo", "0.9.0-1", Some("1.0.0-1")),
    "Downgrading demo (1.0.0-1 -> 0.9.0-1)"
  );
  // The same version on disk is a reinstall, not an upgrade.
  assert_eq!(
    transaction_header("demo", "1.0.0-1", Some("1.0.0-1")),
    "demo 1.0.0-1 is already installed; reinstalling"
  );
}

#[test]
fn only_published_sizes_are_shown() {
  assert!(size_lines(IndexSizes::default()).is_empty(), "nothing published, nothing to say");
  // A zero is a feed declining to measure, not a package of no size.
  assert_eq!(
    size_lines(IndexSizes { installed: Some(0), download: Some(0) }),
    Vec::<String>::new()
  );

  assert_eq!(
    size_lines(IndexSizes { installed: Some(2 * 1024 * 1024), download: Some(1536) }),
    vec!["  Package installed size: 2.0 MiB".to_string(), "  Download size: 1.5 KiB".to_string(),]
  );
}

#[test]
fn the_summary_counts_the_files_the_payload_wrote() {
  // Directories are not files, and the size is the bytes actually written.
  let payload = PayloadInstall {
    files: vec!["/etc/demo.conf".to_string(), "/usr/bin/demo".to_string()],
    dirs: vec!["/etc".to_string()],
    size: 1536,
  };

  assert_eq!(install_summary(Lang::EnUs, &payload), "Installed 2 files, 1.5 KiB");
}

#[test]
fn the_precheck_summary_names_each_source_it_found() {
  let report = json!({
    "kmod": [{"name": "kmod-tun", "status": "feed"}],
    "base": [{"name": "present", "status": "installed"}],
    "app": [
      {"name": "helper", "status": "packaged"},
      {"name": "extra", "status": "feed", "optional": true},
    ],
  });
  let deps = DeclaredDeps { kmod: vec![], base: vec![], app: vec![] };
  // An empty plan covers no optional dependency, which is the default when the
  // user was never asked.
  let optional = optional::Plan::unavailable(&deps, "this backend cannot install them");

  assert_eq!(
    dependency_source_lines(&report, &optional),
    vec!["  kmod-tun (feed)".to_string(), "  helper (packaged)".to_string()],
    "an installed dependency needs nothing, and a declined optional one is not being brought in"
  );
}

#[test]
fn an_upgrade_removes_what_only_the_previous_version_shipped() {
  let mut fixture = PicaFixture::new();
  let manifest = format!("{MANIFEST_DEMO}pkgmgr = pica\n");
  fixture
    .install_without_containers(
      &manifest,
      &[("etc/a.conf", "a\n"), ("etc/b.conf", "b\n")],
      &[],
      &[],
    )
    .expect("version 1 ships a and b");
  fixture
    .install_without_containers(&manifest, &[("etc/a.conf", "a2\n")], &[], &[])
    .expect("version 2 ships only a");

  assert!(fixture.path("etc/a.conf").is_file());
  assert!(!fixture.path("etc/b.conf").exists(), "b belonged only to version 1");
  let record = db_record(&fixture.app, "demo").expect("recorded");
  let files: Vec<String> = serde_json::from_value(record["files"].clone()).expect("files");
  assert!(files.iter().all(|file| !file.ends_with("b.conf")), "{files:?}");
}

#[test]
fn an_upgrade_keeps_a_file_another_package_records() {
  let mut fixture = PicaFixture::new();
  let manifest = format!("{MANIFEST_DEMO}pkgmgr = pica\n");
  fixture
    .install_without_containers(
      &manifest,
      &[("etc/a.conf", "a\n"), ("etc/shared.conf", "s\n")],
      &[],
      &[],
    )
    .expect("version 1");

  // Another package claims the shared file.
  let mut db: Value =
    serde_json::from_str(&fs::read_to_string(&fixture.app.paths.db_file).expect("read db"))
      .expect("parse db");
  // The path exactly as the install recorded it.
  let shared = db["installed"]["demo"]["files"]
    .as_array()
    .expect("files")
    .iter()
    .filter_map(Value::as_str)
    .find(|path| path.ends_with("shared.conf"))
    .expect("shared.conf recorded")
    .to_string();
  db["installed"]["other"] = json!({ "manifest": {"pkgname": "other"}, "files": [shared] });
  fs::write(&fixture.app.paths.db_file, db.to_string()).expect("write db");

  fixture
    .install_without_containers(&manifest, &[("etc/a.conf", "a2\n")], &[], &[])
    .expect("version 2");
  assert!(fixture.path("etc/shared.conf").is_file(), "still recorded by another package");
}

#[test]
fn a_dry_run_upgrade_changes_neither_the_target_nor_the_database() {
  let mut fixture = PicaFixture::new();
  let manifest = format!("{MANIFEST_DEMO}pkgmgr = pica\n");
  fixture
    .install_without_containers(&manifest, &[("etc/a.conf", "v1\n")], &[], &[])
    .expect("version 1");
  let database = fs::read_to_string(&fixture.app.paths.db_file).expect("read db");

  fixture.app.options.dry_run = true;
  let v2 = MANIFEST_DEMO.replace("pkgver = 1.0.0", "pkgver = 2.0.0");
  fixture
    .install_without_containers(
      &format!("{v2}pkgmgr = pica\n"),
      &[("etc/a.conf", "v2\n"), ("etc/b.conf", "new\n")],
      &[],
      &[],
    )
    .expect("a dry run succeeds");

  assert_eq!(fs::read_to_string(fixture.path("etc/a.conf")).expect("read a"), "v1\n");
  assert!(!fixture.path("etc/b.conf").exists(), "nothing new is written");
  assert_eq!(fs::read_to_string(&fixture.app.paths.db_file).expect("read db"), database);
}

#[test]
fn a_dry_run_install_leaves_no_record_and_no_file() {
  let mut fixture = PicaFixture::new();
  fixture.app.options.dry_run = true;
  fixture
    .install_without_containers(
      &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
      &[("etc/a.conf", "v1\n")],
      &[],
      &[],
    )
    .expect("a dry run succeeds");

  assert!(!fixture.path("etc/a.conf").exists());
  assert!(db_record(&fixture.app, "demo").is_none());
}

#[test]
fn a_dry_run_removal_keeps_the_package() {
  let mut fixture = PicaFixture::new();
  fixture
    .install_without_containers(
      &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
      &[("etc/a.conf", "v1\n")],
      &[],
      &[],
    )
    .expect("install");

  fixture.app.options.dry_run = true;
  crate::commands::remove::pkg(&mut fixture.app, "demo").expect("a dry run succeeds");

  assert!(fixture.path("etc/a.conf").is_file());
  assert!(db_record(&fixture.app, "demo").is_some());
}
