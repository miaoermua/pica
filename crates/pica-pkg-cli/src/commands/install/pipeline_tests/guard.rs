//! The payload guard around Pica's own state.

use super::*;
use pretty_assertions::assert_eq;

#[test]
fn a_payload_that_ships_pica_own_database_is_refused_before_anything_is_written() {
  // A package that writes `/var/lib/pica/db.json` is writing the record this
  // very install reads and rewrites. Nothing legitimate does it, so the
  // payload is refused while the database is still the operator's.
  let mut fixture = PicaFixture::new();
  let state = fixture.target.as_path().join("var/lib/pica");
  fs::create_dir_all(&state).expect("mkdir state");
  let database = state.join("db.json");
  fs::write(&database, "{\"schema\":1,\"installed\":{}}\n").expect("existing database");
  let before = fs::read_to_string(&database).expect("read");
  // A hard link tells "untouched" apart from "rewritten with the same bytes":
  // putting a copy back is a rename of a fresh inode, which leaves `db.json`
  // with one link while the sibling keeps the old one.
  let sibling = state.join("db.json.link");
  fs::hard_link(&database, &sibling).expect("hard link the database");
  fixture.app.paths.db_file = database.clone();
  fixture.app.paths.state_dir = state.clone();
  fixture.app.paths.index_file = state.join("index.json");
  fixture.app.paths.report_file = state.join("install-report.json");
  fixture.app.paths.lock_file = state.join("db.lck");

  let err = fixture
    .install_without_containers(
      &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
      &[("var/lib/pica/db.json", "{\"schema\":1,\"installed\":{}}\n")],
      &[],
      &[],
    )
    .expect_err("a payload may not write the database");

  assert!(err.message.contains("database"), "{}", err.message);
  assert_eq!(fs::read_to_string(&database).expect("read"), before, "the database is untouched");
  assert_eq!(
    fs::metadata(&database).expect("db").ino(),
    fs::metadata(&sibling).expect("sibling").ino(),
    "the refusal must come before the copy, or a rollback rewrites a file nothing touched"
  );
  assert!(
    !fixture.app.paths.cache_dir.join("backup").exists(),
    "a payload refused before the walk has nothing to back up"
  );
  assert!(
    !fixture.target.as_path().join("etc").exists(),
    "the payload must be refused before the walk starts"
  );
}

#[test]
fn a_payload_that_ships_the_lock_directory_is_refused() {
  // The lock is `<state>/db.lck.d/pid`. A payload that writes a pid there makes
  // the lock name a process that is not running, so the next `pica` reclaims it
  // while this install is still halfway through.
  let mut fixture = PicaFixture::new();
  let state = fixture.target.as_path().join("var/lib/pica");
  fs::create_dir_all(&state).expect("mkdir state");
  fixture.app.paths.db_file = state.join("db.json");
  fixture.app.paths.state_dir = state.clone();
  fixture.app.paths.index_file = state.join("index.json");
  fixture.app.paths.report_file = state.join("install-report.json");
  fixture.app.paths.lock_file = state.join("db.lck");

  let err = fixture
    .install_without_containers(
      &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
      &[("var/lib/pica/db.lck.d/pid", "1\n"), ("etc/demo.conf", "payload\n")],
      &[],
      &[],
    )
    .expect_err("a payload may not write the lock directory");

  assert!(err.message.contains("lock directory"), "{}", err.message);
  assert!(!state.join("db.lck.d").exists(), "no lock directory was created");
  assert!(
    !fixture.path("etc/demo.conf").exists(),
    "the refusal comes before any of the payload is deployed"
  );
}

#[test]
fn a_payload_that_ships_another_packages_lifecycle_script_is_refused() {
  // Kept scripts are executed as root by `-R` and `-U`, and they live in
  // `/usr/lib/pica/scripts/<pkgname>/`. A payload that can write one can plant
  // or replace a hook for a package it is not.
  let mut fixture = PicaFixture::new();
  let victim = fixture.target.as_path().join("usr/lib/pica/scripts/victim/install.sh");
  write_file(&victim, "#!/bin/sh\necho the installed one\n");

  let err = fixture
    .install_without_containers(
      &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
      &[("usr/lib/pica/scripts/victim/install.sh", "#!/bin/sh\nrm -rf /\n")],
      &[],
      &[],
    )
    .expect_err("a payload may not write another package's hook");

  assert!(err.message.contains("kept scripts"), "{}", err.message);
  assert_eq!(
    fs::read_to_string(&victim).expect("read"),
    "#!/bin/sh\necho the installed one\n",
    "the hook `-U` will run is still the one that was there"
  );
}

#[test]
fn a_payload_that_writes_through_a_symlink_into_pica_state_is_refused() {
  // `usr/lib/demo` is a symlink to the state directory, so `usr/lib/demo/db.json`
  // *is* the database. The name says `usr/lib`; the kernel says `/var/lib/pica`.
  // Which one the guard believes is the whole question, and it believes the
  // filesystem.
  let mut fixture = PicaFixture::new();
  let (state, database) = state_with_a_database(&mut fixture);
  let sibling = state.join("db.json.link");
  fs::hard_link(&database, &sibling).expect("hard link the database");
  let alias = fixture.path("usr/lib/demo");
  fs::create_dir_all(alias.parent().expect("parent")).expect("mkdir usr/lib");
  symlink("../../var/lib/pica", &alias).expect("link the state directory");

  let err = fixture
    .install_without_containers(
      &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
      &[
        ("usr/lib/demo/db.json", "{\"schema\":1,\"installed\":{}}\n"),
        ("etc/demo.conf", "payload\n"),
      ],
      &[],
      &[],
    )
    .expect_err("a payload may not write the database through a symlink");

  assert!(err.message.contains("database"), "{}", err.message);
  assert_eq!(
    fs::read_to_string(&database).expect("read"),
    "{\"schema\":1,\"installed\":{}}\n",
    "the database still holds what the operator left"
  );
  assert_eq!(
    fs::metadata(&database).expect("db").ino(),
    fs::metadata(&sibling).expect("sibling").ino(),
    "the refusal comes before the copy, so the database is not a fresh inode"
  );
  assert!(!fixture.path("etc/demo.conf").exists(), "nothing of the payload was deployed");
}

#[test]
fn a_payload_that_writes_through_a_dangling_symlink_into_pica_state_is_refused() {
  // The link is there and the state directory is not. `create_dir_all` would
  // follow the link and build the directory wherever it names, which is the
  // order that matters: the symlink comes first, the write second, and the guard
  // refuses on where the write would land rather than on what exists yet.
  let mut fixture = PicaFixture::new();
  let (state, _database) = state_with_a_database(&mut fixture);
  fs::remove_dir_all(&state).expect("take the state directory away");
  let alias = fixture.path("usr/lib/demo");
  fs::create_dir_all(alias.parent().expect("parent")).expect("mkdir usr/lib");
  symlink("../../var/lib/pica", &alias).expect("link the state directory");

  let err = fixture
    .install_without_containers(
      &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
      &[
        ("usr/lib/demo/db.json", "{\"schema\":1,\"installed\":{}}\n"),
        ("etc/demo.conf", "payload\n"),
      ],
      &[],
      &[],
    )
    .expect_err("a payload may not create Pica's state through a symlink");

  assert!(err.message.contains("database"), "{}", err.message);
  // Pica makes its own state directory when it opens the store; what matters is
  // that the *payload* stopped at the guard, before the walk wrote anything.
  assert!(!fixture.path("etc/demo.conf").exists(), "nothing of the payload was deployed");
}

#[test]
fn a_payload_that_plants_a_symlink_into_pica_state_is_refused() {
  // A link that only *points* at the state directory writes nothing today. It is
  // what the next install of this package would write through, so it is refused
  // now, while it is still a name and not a way in.
  let mut fixture = PicaFixture::new();
  let _ = state_with_a_database(&mut fixture);

  let err = fixture
    .install_without_containers(
      &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
      &[("etc/demo.conf", "payload\n")],
      &[("opt/alias", "../var/lib/pica")],
      &[],
    )
    .expect_err("a payload may not plant an alias for Pica's state");

  assert!(err.message.contains("state directory"), "{}", err.message);
  assert!(!fixture.path("opt/alias").exists(), "the alias was never created");
  assert!(!fixture.path("etc/demo.conf").exists(), "nothing of the payload was deployed");
}

#[test]
fn a_payload_is_refused_whichever_order_the_walk_takes_its_entries_in() {
  // `root/` is walked in name order, and a guard that decided entry by entry on
  // the way past would answer differently depending on where the names sort. The
  // check is about the destination, not about what was seen before it: `var`
  // sorts after `etc` and before `zzz`, and both orders stop before the walk.
  for harmless in ["etc/aaa.conf", "zzz/zzz.conf"] {
    let mut fixture = PicaFixture::new();
    let (_, database) = state_with_a_database(&mut fixture);

    let err = fixture
      .install_without_containers(
        &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
        &[(harmless, "fine\n"), ("var/lib/pica/db.json", "{\"schema\":1,\"installed\":{}}\n")],
        &[],
        &[],
      )
      .expect_err("a payload may not write the database");

    assert!(err.message.contains("database"), "{harmless}: {}", err.message);
    assert!(!fixture.path(harmless).exists(), "{harmless}: refused before the walk starts");
    assert_eq!(
      fs::read_to_string(&database).expect("read"),
      "{\"schema\":1,\"installed\":{}}\n",
      "{harmless}: the database is untouched"
    );
  }
}

#[test]
fn a_payload_that_writes_through_a_symlink_elsewhere_is_allowed() {
  // Not every symlink in the way is Pica's: `usr/lib/demo` pointing at a version
  // directory is how a package with two versions installed stays addressable.
  // The guard has to tell that apart from the state directory, or it breaks a
  // layout it was never meant to police.
  let mut fixture = PicaFixture::new();
  let _ = state_with_a_database(&mut fixture);
  let version = fixture.path("opt/demo-1.0");
  fs::create_dir_all(&version).expect("mkdir version");
  let alias = fixture.path("usr/lib/demo");
  fs::create_dir_all(alias.parent().expect("parent")).expect("mkdir usr/lib");
  symlink("../../opt/demo-1.0", &alias).expect("link the version directory");

  fixture
    .install_without_containers(
      &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
      &[("usr/lib/demo/app.conf", "ok\n")],
      &[],
      &[],
    )
    .expect("a symlink that leads somewhere else is the package's business");

  assert_eq!(
    fs::read_to_string(version.join("app.conf")).expect("read"),
    "ok\n",
    "the file lands where the link points, as the walk meant it to"
  );
  assert!(
    fs::symlink_metadata(&alias).expect("metadata").is_symlink(),
    "the link itself stays a link"
  );
}
