//! Rolling back a failed install.

use super::*;
use pretty_assertions::assert_eq;

#[test]
fn a_failed_hook_rolls_the_container_back_out() {
  let mut fixture = PicaFixture::new();

  let err = fixture
    .install_without_containers(
      &format!("{MANIFEST_DEMO}pkgmgr = pica\nscript_install = scripts/fail\n"),
      &[("etc/demo.conf", "overlay\n")],
      &[],
      &[("scripts/fail", "#!/bin/sh\nexit 1\n")],
    )
    .expect_err("step 6 fails");

  // A rollback that finishes cleanly adds nothing to the error: it is the
  // hook's own failure, unchanged. Only an incomplete one rewrites it.
  assert!(err.message.contains("sh failed"), "{}", err.message);
  assert!(!err.message.contains("rollback incomplete"), "{}", err.message);
  assert!(
    !fixture.path("usr/lib/demo/helper").exists(),
    "step 5 already unpacked this container, so the rollback has to take it back out"
  );
  assert!(
    !fixture.path("usr/lib/demo").exists(),
    "the directories the container created are empty again and must go too"
  );
  assert!(!fixture.path("etc/demo.conf").exists());
  assert!(db_record(&fixture.app, "demo").is_none());
}

#[test]
fn a_rejected_payload_rolls_back_what_it_already_copied() {
  let mut fixture = PicaFixture::new();

  let err = fixture
    .install_without_containers(
      &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
      &[("aaa.conf", "copied before the walk failed\n")],
      &[("zzz-escape", "/etc/passwd")],
      &[],
    )
    .expect_err("step 7 fails");

  assert!(err.message.contains("escapes the target root"), "{}", err.message);
  assert!(
    !fixture.path("usr/lib/demo/helper").exists(),
    "the container from step 5 must be taken back out"
  );
  assert!(
    !fixture.path("aaa.conf").exists(),
    "the walker reports nothing when it fails, so the file it had already copied is only \
     reachable through the paths this step meant to create that are actually on disk"
  );
  assert!(db_record(&fixture.app, "demo").is_none());
}

#[test]
fn a_failed_upgrade_leaves_the_previous_record_and_the_previous_bytes() {
  let mut fixture = PicaFixture::new();
  fixture
    .install_without_containers(
      &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
      &[("etc/demo.conf", "one\n"), ("etc/keep.conf", "keeper\n")],
      &[],
      &[],
    )
    .expect("the first version installs");
  let before = db_record(&fixture.app, "demo").expect("the first version is recorded");

  // The upgrade gets as far as replacing `etc/demo.conf`, then the env copy
  // dies on a directory sitting at its destination.
  fs::create_dir_all(fixture.path("etc/pica/env.d/demo.env")).expect("block the env copy");
  let err = fixture
    .install_without_containers(
      &format!("{}pkgmgr = pica\n", MANIFEST_DEMO.replace("pkgver = 1.0.0", "pkgver = 2.0.0")),
      &[("etc/demo.conf", "two\n")],
      &[],
      &[("scripts/.env", "PICA_KEEP_SCRIPT_INSTALL=true\n")],
    )
    .expect_err("the upgrade fails");
  assert!(err.message.contains("copy env file failed"), "{}", err.message);

  assert_eq!(
    fs::read_to_string(fixture.path("etc/demo.conf")).expect("read"),
    "one\n",
    "the file the upgrade replaced is back to the bytes the installed version had"
  );
  // The record is not rewritten at all: a rollback that undid everything leaves
  // nothing for `-R` to act on, so the device is what the old record describes.
  assert_eq!(db_record(&fixture.app, "demo").expect("still recorded"), before);
}

#[cfg(unix)]
#[test]
fn an_empty_directory_the_payload_replaced_comes_back_with_its_mode() {
  use std::os::unix::fs::PermissionsExt;

  let mut fixture = PicaFixture::new();
  fs::create_dir_all(fixture.path("etc/demo.conf"))
    .expect("a directory where the payload wants a file");
  fs::set_permissions(fixture.path("etc/demo.conf"), fs::Permissions::from_mode(0o700))
    .expect("give the directory a mode of its own");

  let err = fixture
    .install_without_containers(
      &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
      &[("etc/demo.conf", "a file where the directory was\n")],
      &[("zzz-escape", "/etc/passwd")],
      &[],
    )
    .expect_err("the walk fails after the file replaced the directory");

  assert!(err.message.contains("escapes the target root"), "{}", err.message);
  let path = fixture.path("etc/demo.conf");
  assert!(path.is_dir(), "the directory is what was there before the install");
  assert_eq!(
    fs::metadata(&path).expect("meta").permissions().mode() & 0o777,
    0o700,
    "a directory Pica did not create keeps the mode it had, not the umask's"
  );
}

#[cfg(unix)]
#[test]
fn a_restore_leaves_a_directory_alone_when_the_walk_only_filled_it() {
  // The entry kept an empty directory; the walk then put a file inside it. The
  // record removes that file, so the directory is already what it was — and a
  // restore that tried to recreate it would fail on a directory it cannot
  // empty and report a path that never went missing.
  use std::os::unix::fs::PermissionsExt;

  let work = tempfile::tempdir().expect("temp dir");
  let destination = work.path().join("etc/demo");
  fs::create_dir_all(&destination).expect("mkdir");
  fs::write(destination.join("added"), "written by the walk\n").expect("write");

  let mut backup = PayloadBackup {
    parent: work.path().join("backup"),
    prefix: "demo-".to_string(),
    dir: None,
    entries: vec![BackupEntry {
      destination: destination.clone(),
      kind: BackupKind::EmptyDir { permissions: fs::Permissions::from_mode(0o700) },
      walked: true,
    }],
  };

  let failed = backup.restore();

  assert!(failed.is_empty(), "nothing went missing: {failed:?}");
  assert!(destination.is_dir());
  assert!(
    destination.join("added").is_file(),
    "removing what the walk wrote is the record's job, not the restore's"
  );
}

#[test]
fn a_restore_leaves_a_file_the_walk_never_reached_alone() {
  // Two files are copied out before the walk starts. It replaces the first and
  // stops; the second is exactly what it was. Putting the copy back over the
  // second would rewrite the same bytes into a fresh inode — a hard link to it
  // would come apart, and a large file would be read and written for nothing —
  // so the rollback restores only what the walk reported reaching.
  let work = tempfile::tempdir().expect("temp dir");
  let app = test_app(work.path());
  let first = work.path().join("one.conf");
  let second = work.path().join("two.conf");
  write_file(&first, "old-one\n");
  write_file(&second, "old-two\n");
  let sibling = work.path().join("two.conf.link");
  fs::hard_link(&second, &sibling).expect("hard link the second file");

  let mut backup = PayloadBackup::new(&app, "demo");
  for destination in [&first, &second] {
    let metadata = fs::symlink_metadata(destination).expect("metadata");
    backup.back_up(destination, &metadata).expect("back up");
    // What `snapshot_payload` marks on the entries it takes: these are files the
    // walk may replace, unlike the ones Pica writes itself.
    if let Some(entry) = backup.entries.last_mut() {
      entry.walked = true;
    }
  }

  // What a deploy that reached only the first file leaves behind: the rename is
  // the same one `deploy_file` makes, so the first path is a new inode.
  let staged = work.path().join("one.conf.tmp");
  write_file(&staged, "new-one\n");
  fs::rename(&staged, &first).expect("replace the first file");

  let deployed: HashSet<PathBuf> = [first.clone()].into_iter().collect();
  let failed = backup.restore_replacing(&deployed);
  assert!(failed.is_empty(), "{failed:?}");
  assert_eq!(fs::read_to_string(&first).expect("read"), "old-one\n", "the reached file went back");
  assert_eq!(
    fs::metadata(&second).expect("second").ino(),
    fs::metadata(&sibling).expect("sibling").ino(),
    "the file the walk never reached was not rewritten"
  );
  assert_eq!(fs::read_to_string(&second).expect("read"), "old-two\n");
}

#[test]
fn a_restore_that_failed_partway_puts_the_rest_back_when_it_is_run_again() {
  // A rollback can fail on one path and succeed on the rest. Running it again
  // after the fault is repaired has to finish the job, and has to leave the
  // paths it already restored with the bytes, the mode and the shape they had —
  // not with what the failed install put there.
  let work = tempfile::tempdir().expect("temp dir");
  let app = test_app(work.path());
  let first_dir = work.path().join("first");
  let second_dir = work.path().join("second");
  fs::create_dir_all(&first_dir).expect("mkdir first");
  fs::create_dir_all(&second_dir).expect("mkdir second");
  let first = first_dir.join("one.conf");
  let second = second_dir.join("two.conf");
  write_file(&first, "old-one\n");
  write_file(&second, "old-two\n");
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&second, fs::Permissions::from_mode(0o640)).expect("mode");
  }

  let mut backup = PayloadBackup::new(&app, "demo");
  for destination in [&first, &second] {
    let metadata = fs::symlink_metadata(destination).expect("metadata");
    backup.back_up(destination, &metadata).expect("back up");
  }

  // What the deploy would have done to both files.
  write_file(&first, "new-one\n");
  write_file(&second, "new-two\n");

  // The fault: the directory `second` lives in is gone, so that copy cannot go
  // back. The other one still can, and does.
  fs::remove_dir_all(&second_dir).expect("take the parent away");
  let failed = backup.restore();
  assert_eq!(failed.len(), 1, "{failed:?}");
  assert!(failed[0].contains("two.conf"), "{failed:?}");
  assert_eq!(fs::read_to_string(&first).expect("read"), "old-one\n", "the first one went back");
  assert!(!second.exists(), "the second one could not");

  // The operator repairs the fault and the same rollback runs again. Nothing is
  // deleted, and the file the first run already restored is not replaced with
  // the failed install's bytes.
  fs::create_dir_all(&second_dir).expect("put the parent back");
  let failed = backup.restore();
  assert!(failed.is_empty(), "{failed:?}");
  assert_eq!(fs::read_to_string(&first).expect("read"), "old-one\n");
  assert_eq!(fs::read_to_string(&second).expect("read"), "old-two\n");
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
      fs::metadata(&second).expect("metadata").permissions().mode() & 0o777,
      0o640,
      "the mode the file had comes back with its bytes"
    );
  }
}

#[test]
fn a_database_that_cannot_be_updated_undoes_the_files_it_just_deployed() {
  // The database is the commit point. An install whose record cannot be
  // written is not an install: the files go back to what the old record
  // describes, instead of staying on the device with nothing naming them.
  let mut fixture = PicaFixture::new();
  fixture
    .install_without_containers(
      &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
      &[("etc/demo.conf", "one\n")],
      &[],
      &[],
    )
    .expect("the first version installs");
  let before = db_record(&fixture.app, "demo").expect("recorded");

  // The atomic write needs `db.json.tmp-<pid>`: a directory standing on that
  // name is a write that cannot happen.
  let blocked = fixture.app.paths.db_file.with_file_name(format!(
    "{}.tmp-{}",
    fixture.app.paths.db_file.file_name().expect("name").to_string_lossy(),
    std::process::id()
  ));
  fs::create_dir_all(&blocked).expect("block the database's temporary name");
  assert!(
    pica_pkg_core::io::write_atomic(&fixture.app.paths.db_file, b"{}").is_err(),
    "the block has to be what stops the write"
  );

  fixture
    .install_without_containers(
      &format!("{}pkgmgr = pica\n", MANIFEST_DEMO.replace("pkgver = 1.0.0", "pkgver = 2.0.0")),
      &[("etc/demo.conf", "two\n")],
      &[],
      &[],
    )
    .expect_err("an install that cannot be recorded fails");

  assert_eq!(
    fs::read_to_string(fixture.path("etc/demo.conf")).expect("read"),
    "one\n",
    "the file the failed install replaced is back to what is installed"
  );
  assert_eq!(
    db_record(&fixture.app, "demo").expect("still recorded"),
    before,
    "the record still describes the version that is on the device"
  );
}

#[test]
fn an_install_report_that_cannot_be_written_does_not_undo_the_install() {
  // The database is the record; the report explains it. A report that cannot
  // be written leaves a recorded install and a warning, not a rollback of
  // files that are in place. The report is blocked by occupying the temporary
  // name its atomic write uses — the file itself has to stay readable so the
  // startup initialisation, which writes it when it is missing, gets past.
  let mut fixture = PicaFixture::new();
  fs::create_dir_all(&fixture.app.paths.state_dir).expect("state dir");
  fs::write(&fixture.app.paths.report_file, "{\"schema\": 1, \"reports\": {}}")
    .expect("an existing report");
  let blocked = fixture.app.paths.report_file.with_file_name(format!(
    "{}.tmp-{}",
    fixture.app.paths.report_file.file_name().expect("name").to_string_lossy(),
    std::process::id()
  ));
  fs::create_dir_all(&blocked).expect("block the report's temporary name");

  fixture
    .install_without_containers(
      &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
      &[("etc/demo.conf", "overlay\n")],
      &[],
      &[],
    )
    .expect("the install itself is committed");

  assert_eq!(
    fs::read_to_string(fixture.path("etc/demo.conf")).expect("read"),
    "overlay\n",
    "the file must survive a report that could not be written"
  );
  let record = db_record(&fixture.app, "demo").expect("the database owns the file");
  assert!(
    record["files"]
      .as_array()
      .expect("files")
      .iter()
      .any(|file| file.as_str().is_some_and(|path| path.ends_with("/etc/demo.conf"))),
    "the record must name what was installed: {record}"
  );
}

#[test]
fn an_env_copy_failure_rolls_back_the_payload_it_just_deployed() {
  let mut fixture = PicaFixture::new();

  // The env file's own path is a directory, so `fs::copy` fails with EISDIR.
  // That is deterministic, needs no privileges, and it is the step that runs
  // *after* the `root/` files are already on disk.
  let env_file = fixture.path("etc/pica/env.d/demo.env");
  fs::create_dir_all(&env_file).expect("create the blocking directory");

  let err =
    deploy_with_env(&mut fixture, true).expect_err("the env copy cannot land on a directory");
  assert!(err.message.contains("copy env file failed"), "{}", err.message);

  assert!(
    !fixture.path("etc/demo.conf").exists(),
    "the payload deployed before the failure must be removed again"
  );
  assert!(db_record(&fixture.app, "demo").is_none(), "a failed install leaves no record behind");
}

#[test]
fn an_install_without_an_env_file_still_deploys_its_payload() {
  let mut fixture = PicaFixture::new();

  deploy_with_env(&mut fixture, false).expect("a package without scripts/.env installs");

  assert!(fixture.path("etc/demo.conf").is_file(), "the payload is deployed");
  assert!(db_record(&fixture.app, "demo").is_some(), "and recorded");
}

#[test]
fn a_failed_env_copy_can_be_retried_and_the_retry_uninstalls_cleanly() {
  let mut fixture = PicaFixture::new();
  let env_file = fixture.path("etc/pica/env.d/demo.env");
  fs::create_dir_all(&env_file).expect("create the blocking directory");

  deploy_with_env(&mut fixture, true).expect_err("the first attempt fails");
  assert!(!fixture.path("etc/demo.conf").exists(), "the first attempt leaves no orphan behind");

  fs::remove_dir(&env_file).expect("clear the blocker");
  deploy_with_env(&mut fixture, true).expect("the retry installs");

  let record = db_record(&fixture.app, "demo").expect("the retry is recorded");
  assert!(record.to_string().contains("etc/demo.conf"), "{record}");

  crate::commands::remove::pkg(&mut fixture.app, "demo").expect("uninstall");
  assert!(!fixture.path("etc/demo.conf").exists(), "uninstall removes what the retry deployed");
  assert!(db_record(&fixture.app, "demo").is_none());
}

#[test]
fn a_rollback_restores_the_content_of_a_file_it_replaced() {
  let mut fixture = PicaFixture::new();
  // The artifact ships this path, so the walk replaces it — but it existed
  // before the install started, which makes it not the rollback's to remove.
  // Its content, on the other hand, is the rollback's to put back.
  let existing = fixture.path("etc/demo.conf");
  fs::create_dir_all(existing.parent().expect("parent")).expect("mkdir");
  fs::write(&existing, "original\n").expect("write the pre-existing file");

  let env_file = fixture.path("etc/pica/env.d/demo.env");
  fs::create_dir_all(&env_file).expect("create the blocking directory");

  deploy_with_env(&mut fixture, true).expect_err("the env copy fails");

  assert!(existing.exists(), "a path that was there before the install must survive the rollback");
  assert_eq!(
    fs::read_to_string(&existing).expect("read"),
    "original\n",
    "the copy taken before the walk replaced it"
  );
}

#[test]
fn an_env_dir_failure_rolls_back_the_payload_it_just_deployed() {
  let mut fixture = PicaFixture::new();
  // `/etc/pica` is a regular file here, so creating `/etc/pica/env.d` fails
  // with ENOTDIR — deterministic, and it needs neither privileges nor a full
  // disk. It is the step right after the `root/` files land.
  let blocker = fixture.path("etc/pica");
  fs::create_dir_all(blocker.parent().expect("parent")).expect("mkdir");
  fs::write(&blocker, "not a directory\n").expect("write the blocker");

  deploy_with_env(&mut fixture, true).expect_err("the env directory cannot be created");

  assert!(
    !fixture.path("etc/demo.conf").exists(),
    "the payload deployed before the failure is rolled back"
  );
  assert!(db_record(&fixture.app, "demo").is_none());
}

#[test]
fn a_failed_copy_that_created_the_target_records_it() {
  let dir = tempfile::tempdir().expect("temp dir");
  let target = dir.path().join("demo.env");
  let mut tx = InstallTransaction::new();

  let err = copy_recording_new_target(&mut tx, &target, false, || {
    // The failure a real filesystem only produces on a bad day: the target is
    // opened and written, then the write dies.
    fs::write(&target, b"half-written").expect("write the partial file");
    Err(std::io::Error::other("simulated write failure"))
  })
  .expect_err("the copy fails");

  assert_eq!(err.kind(), std::io::ErrorKind::Other);
  assert_eq!(
    tx.created_records.len(),
    1,
    "the file the failed copy left behind has to reach the rollback"
  );
  assert_eq!(tx.created_records[0].files, vec![target.display().to_string()]);
}

#[test]
fn a_failed_copy_over_an_existing_target_is_not_recorded() {
  let dir = tempfile::tempdir().expect("temp dir");
  let target = dir.path().join("demo.env");
  fs::write(&target, b"original").expect("write the original file");
  let mut tx = InstallTransaction::new();

  copy_recording_new_target(&mut tx, &target, true, || {
    fs::write(&target, b"replaced").expect("overwrite the file");
    Err(std::io::Error::other("simulated write failure"))
  })
  .expect_err("the copy fails");

  assert!(
    tx.created_records.is_empty(),
    "a path that was already on the device is not the rollback's to remove"
  );
  assert!(target.is_file(), "and it is not removed either");
}

#[test]
fn a_failed_copy_that_created_nothing_records_nothing() {
  let dir = tempfile::tempdir().expect("temp dir");
  let target = dir.path().join("demo.env");
  let mut tx = InstallTransaction::new();

  copy_recording_new_target(&mut tx, &target, false, || {
    Err(std::io::Error::other("simulated open failure"))
  })
  .expect_err("the copy fails");

  assert!(tx.created_records.is_empty(), "there is no path to remove");
}

#[test]
fn a_failed_env_copy_over_an_existing_file_records_nothing_and_keeps_the_path() {
  let dir = tempfile::tempdir().expect("temp dir");
  let target = dir.path().join("demo.env");
  fs::write(&target, b"original\n").expect("write the original env file");
  let mut tx = InstallTransaction::new();

  copy_recording_new_target(&mut tx, &target, true, || {
    // Truncate and rewrite, then die — what a partial copy leaves behind.
    fs::write(&target, b"partial").expect("rewrite the target");
    Err(std::io::Error::other("simulated write failure"))
  })
  .expect_err("the copy fails");

  assert!(
    tx.created_records.is_empty(),
    "a path that existed before the copy is not this install's to remove"
  );

  // What the record alone does with this path: nothing. The bytes stay as the
  // failed copy left them, which is why the *transaction* keeps a copy of the
  // env file before copying it — see
  // `a_failed_env_copy_restores_the_file_that_was_already_there`.
  let mut recorded = PayloadInstall::default();
  for record in &tx.created_records {
    recorded.files.extend(record.files.iter().cloned());
    recorded.dirs.extend(record.dirs.iter().cloned());
  }
  let _ = remove_payload(&recorded);

  assert!(target.is_file(), "the pre-existing path survives the rollback");
  assert_eq!(
    fs::read_to_string(&target).expect("read the env file"),
    "partial",
    "the record on its own restores no content"
  );
}

#[test]
fn a_mid_payload_failure_restores_a_file_that_was_already_there() {
  let mut fixture = PicaFixture::new();
  // The walk goes in name order: `aaa.conf` lands, `etc/demo.conf` — already
  // on the device — is replaced, and then the escaping symlink fails it. The
  // path this install created is removed, and the one it replaced goes back to
  // the copy taken before the walk started.
  let existing = fixture.path("etc/demo.conf");
  fs::create_dir_all(existing.parent().expect("parent")).expect("mkdir");
  fs::write(&existing, "original\n").expect("write the pre-existing file");

  let err = fixture
    .install_without_containers(
      &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
      &[("aaa.conf", "copied first\n"), ("etc/demo.conf", "payload\n")],
      &[("zzz-escape", "/etc/passwd")],
      &[],
    )
    .expect_err("the escaping symlink fails the walk");

  assert!(err.message.contains("escapes the target root"), "{}", err.message);
  assert!(
    existing.exists(),
    "a path that was there before the install is not the rollback's to remove"
  );
  assert_eq!(
    fs::read_to_string(&existing).expect("read"),
    "original\n",
    "and its content is not the artifact's either"
  );
  assert!(
    !fixture.path("aaa.conf").exists(),
    "the file this walk created before failing is removed"
  );
}
