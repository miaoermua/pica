//! Copies of replaced files and putting them back.

use super::*;
use pretty_assertions::assert_eq;

#[test]
fn a_rollback_after_a_full_deploy_restores_every_file_it_replaced() {
  let fixture = PicaFixture::new();
  for (relative, contents) in
    [("etc/demo.conf", "operator one\n"), ("etc/app/settings.json", "operator two\n")]
  {
    write_file(&fixture.path(relative), contents);
  }

  let mut deployed = Deployed::new(
    fixture,
    &[("etc/demo.conf", "payload one\n"), ("etc/app/settings.json", "payload two\n")],
    &[],
  )
  .expect("a payload of plain files deploys");
  assert_eq!(
    fs::read_to_string(deployed.path("etc/demo.conf")).expect("read"),
    "payload one\n",
    "the deploy really did replace what was there"
  );

  let leftovers = deployed.rollback();
  assert!(leftovers.is_empty(), "the rollback should undo everything: {leftovers:?}");
  assert_eq!(fs::read_to_string(deployed.path("etc/demo.conf")).expect("read"), "operator one\n");
  assert_eq!(
    fs::read_to_string(deployed.path("etc/app/settings.json")).expect("read"),
    "operator two\n",
    "every file the walk replaced comes back, not only the first"
  );
}

#[test]
fn a_rollback_restores_the_env_file_the_install_overwrote() {
  let fixture = PicaFixture::new();
  write_file(&fixture.path("etc/pica/env.d/demo.env"), "PICA_KEEP_SCRIPT_INSTALL=true\noperator\n");
  write_file(&fixture.path("etc/demo.conf"), "operator\n");

  let mut deployed = Deployed::new(
    fixture,
    &[("etc/demo.conf", "payload\n")],
    &[("scripts/.env", "PICA_KEEP_SCRIPT_INSTALL=true\nnew\n")],
  )
  .expect("the payload and the env file both land");
  assert_eq!(
    fs::read_to_string(deployed.path("etc/pica/env.d/demo.env")).expect("read"),
    "PICA_KEEP_SCRIPT_INSTALL=true\nnew\n",
    "the artifact's env file replaced the operator's"
  );

  let leftovers = deployed.rollback();
  assert!(leftovers.is_empty(), "the rollback should undo everything: {leftovers:?}");
  assert_eq!(
    fs::read_to_string(deployed.path("etc/pica/env.d/demo.env")).expect("read"),
    "PICA_KEEP_SCRIPT_INSTALL=true\noperator\n",
    "a file Pica owns is still the operator's file, and it goes back like any other"
  );
  assert_eq!(fs::read_to_string(deployed.path("etc/demo.conf")).expect("read"), "operator\n");
}

#[test]
fn a_symlink_the_payload_replaced_comes_back_as_a_symlink() {
  let fixture = PicaFixture::new();
  write_file(&fixture.path("etc/keep.conf"), "operator\n");
  symlink("keep.conf", fixture.path("etc/demo.conf")).expect("create the pre-existing link");

  let mut deployed =
    Deployed::new(fixture, &[("etc/demo.conf", "payload\n")], &[]).expect("the file lands");
  assert!(
    !deployed.path("etc/demo.conf").is_symlink(),
    "the rename replaced the link with the artifact's file"
  );

  let leftovers = deployed.rollback();
  assert!(leftovers.is_empty(), "the rollback should undo everything: {leftovers:?}");
  assert_eq!(
    fs::read_link(deployed.path("etc/demo.conf")).expect("read the link"),
    Path::new("keep.conf"),
    "what comes back is the link itself, not a copy of what it pointed at"
  );
  assert_eq!(
    fs::read_to_string(deployed.path("etc/keep.conf")).expect("read"),
    "operator\n",
    "and nothing was ever written through it"
  );
}

#[test]
fn an_empty_directory_the_payload_replaced_comes_back_as_a_directory() {
  let fixture = PicaFixture::new();
  fs::create_dir_all(fixture.path("etc")).expect("create the parent");
  fs::create_dir(fixture.path("etc/demo.conf")).expect("create the empty directory");

  let mut deployed =
    Deployed::new(fixture, &[("etc/demo.conf", "payload\n")], &[]).expect("the file lands");
  assert!(deployed.path("etc/demo.conf").is_file(), "a file took the empty directory's place");

  let leftovers = deployed.rollback();
  assert!(leftovers.is_empty(), "the rollback should undo everything: {leftovers:?}");
  assert!(
    deployed.path("etc/demo.conf").is_dir(),
    "the path is a directory again, which is what it was"
  );
}

#[test]
fn a_hard_linked_destination_keeps_its_other_name_and_its_content() {
  let fixture = PicaFixture::new();
  let original = fixture.path("etc/demo.conf");
  write_file(&original, "operator\n");
  let alias = fixture.path("etc/alias.conf");
  fs::hard_link(&original, &alias).expect("create the second name");

  let mut deployed =
    Deployed::new(fixture, &[("etc/demo.conf", "payload\n")], &[]).expect("the file lands");
  assert_eq!(
    fs::read_to_string(&alias).expect("read"),
    "operator\n",
    "the rename put a new inode at the destination, so the other name never changed"
  );

  let leftovers = deployed.rollback();
  assert!(leftovers.is_empty(), "the rollback should undo everything: {leftovers:?}");
  assert_eq!(fs::read_to_string(deployed.path("etc/demo.conf")).expect("read"), "operator\n");
  assert_eq!(
    fs::read_to_string(&alias).expect("read"),
    "operator\n",
    "both names hold the bytes the device had; that they are now two inodes is what \
     restoring one file by rename means"
  );
}

#[test]
fn a_restore_that_cannot_put_a_file_back_keeps_the_copies_and_names_the_path() {
  let fixture = PicaFixture::new();
  write_file(&fixture.path("etc/demo.conf"), "operator\n");
  let mut deployed =
    Deployed::new(fixture, &[("etc/demo.conf", "payload\n")], &[]).expect("the file lands");
  let stores = backup_stores(&deployed.fixture);
  assert_eq!(stores.len(), 1, "the deploy kept exactly one store: {stores:?}");

  deployed.make_the_restore_fail("etc/demo.conf");
  let leftovers = deployed.rollback();

  assert_eq!(leftovers.len(), 1, "the file that could not go back is reported: {leftovers:?}");
  assert!(leftovers[0].starts_with("content:"), "it is named as content: {leftovers:?}");
  assert!(leftovers[0].contains("etc/demo.conf"), "with its path: {leftovers:?}");
  assert_eq!(
    backup_stores(&deployed.fixture),
    stores,
    "the copies stay where the message says they are — they are the only version of that \
     file left on the device"
  );
}

#[test]
fn a_completed_restore_leaves_nothing_to_do_a_second_time() {
  let fixture = PicaFixture::new();
  write_file(&fixture.path("etc/demo.conf"), "operator\n");
  let mut deployed =
    Deployed::new(fixture, &[("etc/demo.conf", "payload\n")], &[]).expect("the file lands");

  assert!(deployed.rollback().is_empty(), "the first rollback undid everything");
  let second = deployed.rollback();
  assert!(
    second.is_empty(),
    "a rollback of an install already rolled back reports nothing: {second:?}"
  );
  assert_eq!(
    fs::read_to_string(deployed.path("etc/demo.conf")).expect("read"),
    "operator\n",
    "and does not disturb what is already back"
  );
  assert!(backup_stores(&deployed.fixture).is_empty(), "the store went with the first rollback");
}

#[test]
fn a_successful_install_removes_the_copies_it_took() {
  let mut fixture = PicaFixture::new();
  write_file(&fixture.path("etc/demo.conf"), "operator\n");

  fixture
    .install_without_containers(
      &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
      &[("etc/demo.conf", "payload\n")],
      &[],
      &[],
    )
    .expect("the install completes");

  assert_eq!(
    fs::read_to_string(fixture.path("etc/demo.conf")).expect("read"),
    "payload\n",
    "the install is the one that wins"
  );
  assert!(backup_stores(&fixture).is_empty(), "a committed install collects its own copies");
}

#[test]
fn a_recorded_install_names_the_file_it_replaced_and_says_nothing_about_the_copies() {
  let mut fixture = PicaFixture::new();
  write_file(&fixture.path("etc/demo.conf"), "operator\n");

  fixture
    .install_without_containers(
      &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
      &[("etc/demo.conf", "payload\n")],
      &[],
      &[],
    )
    .expect("the install completes");

  // The record is the same one it always was: the file this install wrote,
  // whether or not that file was already there. Keeping a copy of the previous
  // content adds no path to it, which is why the copy is not installed, not
  // counted, and not something `-R` can delete.
  let record = db_record(&fixture.app, "demo").expect("the install is recorded");
  let text = record.to_string();
  assert!(text.contains("etc/demo.conf"), "the replaced path is owned: {text}");
  assert!(!text.contains("cache/backup"), "the copy is not: {text}");

  crate::commands::remove::pkg(&mut fixture.app, "demo").expect("uninstall");
  assert!(
    !fixture.path("etc/demo.conf").exists(),
    "and the uninstall that follows the existing ownership policy still removes it"
  );
}

#[test]
fn an_install_that_replaces_nothing_never_makes_a_backup_directory() {
  let mut fixture = PicaFixture::new();

  fixture
    .install_without_containers(
      &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
      &[("etc/demo.conf", "payload\n")],
      &[],
      &[],
    )
    .expect("a first install completes");

  assert!(
    !fixture.app.paths.cache_dir.join("backup").exists(),
    "an install that destroys nothing copies nothing, so it creates no directory either"
  );
}

#[cfg(unix)]
#[test]
fn the_copies_are_only_readable_by_the_user_who_installed_them() {
  use std::os::unix::fs::PermissionsExt;

  let fixture = PicaFixture::new();
  let original = fixture.path("etc/demo.conf");
  write_file(&original, "operator\n");
  fs::set_permissions(&original, fs::Permissions::from_mode(0o644))
    .expect("make the device file world-readable");

  let mut deployed =
    Deployed::new(fixture, &[("etc/demo.conf", "payload\n")], &[]).expect("the file lands");
  // A restore that fails keeps the store, which is the only moment the copies
  // are on disk while a test can still look at them.
  deployed.make_the_restore_fail("etc/demo.conf");
  assert_eq!(deployed.rollback().len(), 1, "kept so the copies are still there");

  let store = backup_stores(&deployed.fixture).into_iter().next().expect("one store");
  let mode = fs::metadata(&store).expect("stat the store").permissions().mode() & 0o777;
  assert_eq!(mode, 0o700, "the store itself is the installer's alone");
  let copies: Vec<PathBuf> =
    fs::read_dir(&store).expect("read the store").flatten().map(|entry| entry.path()).collect();
  assert!(!copies.is_empty(), "the copy of the replaced file is in the store");
  for copy in copies {
    assert_eq!(
      fs::symlink_metadata(&copy).expect("stat the copy").permissions().mode() & 0o777,
      0o600,
      "{} was world-readable on the device; the copy of it is not",
      copy.display()
    );
  }
}

#[test]
fn a_backup_that_cannot_be_written_stops_the_install_before_anything_is_written() {
  let mut fixture = PicaFixture::new();
  write_file(&fixture.path("etc/demo.conf"), "operator\n");
  write_file(&fixture.path("aaa.conf"), "operator too\n");
  // The cache directory's backup root is a regular file, so making a store in
  // it fails with `ENOTDIR` for any user, root included.
  let blocker = fixture.app.paths.cache_dir.join("backup");
  write_file(&blocker, "not a directory\n");

  let err = fixture
    .install_without_containers(
      &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
      &[("etc/demo.conf", "payload\n"), ("aaa.conf", "payload too\n")],
      &[],
      &[],
    )
    .expect_err("the copies cannot be kept");

  assert!(
    err.message.contains("backup"),
    "the message names the store it could not make: {}",
    err.message
  );
  assert_eq!(
    fs::read_to_string(fixture.path("etc/demo.conf")).expect("read"),
    "operator\n",
    "the file the walk would have replaced is untouched"
  );
  assert_eq!(
    fs::read_to_string(fixture.path("aaa.conf")).expect("read"),
    "operator too\n",
    "and so is the one it would have replaced"
  );
  assert!(db_record(&fixture.app, "demo").is_none(), "and nothing was recorded");
}

#[cfg(unix)]
#[test]
fn a_socket_at_a_payload_destination_stops_the_install_before_anything_is_written() {
  use std::os::unix::fs::FileTypeExt;

  let mut fixture = PicaFixture::new();
  let blocker = fixture.path("etc/demo.conf");
  write_file(&fixture.path("etc/other.conf"), "operator\n");
  std::os::unix::net::UnixListener::bind(&blocker).expect("bind a socket at the destination");

  let err = fixture
    .install_without_containers(
      &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
      &[("etc/demo.conf", "payload\n"), ("aaa.conf", "payload too\n")],
      &[],
      &[],
    )
    .expect_err("a socket cannot be put back from a copy");

  assert!(err.message.contains("neither a file, a symlink nor a directory"), "{}", err.message);
  assert!(
    fs::symlink_metadata(&blocker).expect("stat").file_type().is_socket(),
    "the socket is exactly what it was"
  );
  assert!(
    !fixture.path("aaa.conf").exists(),
    "not even the file the walk would have reached first was written"
  );
  assert_eq!(
    fs::read_to_string(fixture.path("etc/other.conf")).expect("read"),
    "operator\n",
    "the device kept every byte it had"
  );
  assert!(db_record(&fixture.app, "demo").is_none());
}

#[test]
fn a_finish_that_cannot_remove_the_copies_leaves_them_and_says_so() {
  let dir = tempfile::tempdir().expect("temp dir");
  let mut app = test_app(dir.path());
  let original = dir.path().join("device.conf");
  fs::write(&original, "operator\n").expect("write the device file");

  let mut backup = PayloadBackup::new(&app, "demo");
  let metadata = fs::symlink_metadata(&original).expect("stat the device file");
  backup.back_up(&original, &metadata).expect("keep a copy");
  let store = backup.dir.as_ref().expect("the copy made a store").path().to_path_buf();

  let mut tx = InstallTransaction::new();
  tx.payload_backup = Some(backup);
  tx.finish_with(&mut app, |_| Err(std::io::Error::other("simulated copy removal failure")));

  assert!(store.is_dir(), "the install completed, but the copies were not deleted with it");
  assert_eq!(
    fs::read_dir(&store).expect("read the store").flatten().count(),
    1,
    "and the copy it made of the file it replaced is still inside"
  );
}

#[test]
fn a_failed_env_copy_restores_the_file_that_was_already_there() {
  let dir = tempfile::tempdir().expect("temp dir");
  let app = test_app(dir.path());
  let target = dir.path().join("demo.env");
  fs::write(&target, "original\n").expect("write the env file the operator left");

  // The same store step 7 hands the env file to. It is taken directly here
  // because the copy failure this test needs — a write that dies after
  // truncating — is not something a filesystem can be made to do on demand.
  let mut backup = PayloadBackup::new(&app, "demo");
  backup
    .back_up(&target, &fs::symlink_metadata(&target).expect("stat the env file"))
    .expect("keep the copy");
  let mut tx = InstallTransaction::new();
  tx.payload_backup = Some(backup);

  copy_recording_new_target(&mut tx, &target, true, || {
    fs::write(&target, "partial").expect("truncate and rewrite the env file");
    Err(std::io::Error::other("simulated write failure"))
  })
  .expect_err("the copy fails");
  assert_eq!(
    fs::read_to_string(&target).expect("read"),
    "partial",
    "the record on its own leaves the half-written file in place"
  );

  let failed = tx.payload_backup.as_mut().expect("the transaction holds the store").restore();
  assert!(failed.is_empty(), "the env file goes back: {failed:?}");
  assert_eq!(
    fs::read_to_string(&target).expect("read"),
    "original\n",
    "the copy taken before the copy is what puts it back"
  );
  assert!(
    tx.created_records.is_empty(),
    "and the env file, which existed already, is still not this install's to remove"
  );
}
