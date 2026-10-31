//! Kept lifecycle scripts across installs and upgrades.

use super::*;
use pretty_assertions::assert_eq;

#[test]
fn a_failure_after_the_first_kept_script_is_copied_rolls_it_back() {
  let mut fixture = PicaFixture::new();
  // `update` is copied first; `remove` is declared but absent from the
  // artifact, so the loop stops right after one successful copy. No lifecycle
  // hook runs on a first install here, which keeps the rollback limited to
  // the file this install actually wrote.
  let manifest = format!(
    "{MANIFEST_DEMO}pkgmgr = pica\nscript_update = scripts/update\n\
     script_remove = scripts/remove\n"
  );
  let err = fixture
    .install_without_containers(
      &manifest,
      &[],
      &[],
      &[
        ("scripts/update", "#!/bin/sh\nexit 0\n"),
        ("scripts/.env", "PICA_KEEP_SCRIPT_UPDATE=true\nPICA_KEEP_SCRIPT_REMOVE=true\n"),
      ],
    )
    .expect_err("the declared remove script is not in the artifact");

  assert!(err.message.contains("script not found in package"), "{}", err.message);
  assert!(
    !fixture.path("usr/lib/pica/scripts/demo/scripts/update").exists(),
    "the script copied before the failure is rolled back"
  );
}

#[test]
fn a_failed_upgrade_restores_the_previous_scripts() {
  let mut fixture = PicaFixture::new();
  let manifest = format!(
    "{MANIFEST_DEMO}pkgmgr = pica\nscript_install = scripts/install\n\
     script_remove = scripts/remove\n"
  );
  let env = ("scripts/.env", "PICA_KEEP_SCRIPT_INSTALL=true\nPICA_KEEP_SCRIPT_REMOVE=true\n");
  let install_script = "usr/lib/pica/scripts/demo/scripts/install";
  let remove_script = "usr/lib/pica/scripts/demo/scripts/remove";

  // Version 1 keeps both scripts, so the upgrade has old files to replace.
  fixture
    .install_without_containers(
      &manifest,
      &[],
      &[],
      &[
        ("scripts/install", "#!/bin/sh\necho old-install\n"),
        ("scripts/remove", "#!/bin/sh\necho old-remove\n"),
        env,
      ],
    )
    .expect("the first install keeps both scripts");

  // Version 2 drops `scripts/remove`: the new `install` is copied, then the
  // loop stops. The old directory was moved aside, not deleted.
  let err = fixture
    .install_without_containers(
      &manifest,
      &[],
      &[],
      &[("scripts/install", "#!/bin/sh\necho new-install\n"), env],
    )
    .expect_err("version 2 does not ship the declared remove script");

  assert!(err.message.contains("script not found in package"), "{}", err.message);
  assert_eq!(
    fs::read_to_string(fixture.path(install_script)).expect("read the old install script"),
    "#!/bin/sh\necho old-install\n",
    "the old script is back, not the new copy"
  );
  assert_eq!(
    fs::read_to_string(fixture.path(remove_script)).expect("read the old remove script"),
    "#!/bin/sh\necho old-remove\n",
    "the script the new version no longer ships is restored too"
  );
  assert_eq!(backup_stores(&fixture), Vec::<PathBuf>::new(), "a full restore keeps no copies");
}

#[test]
fn a_failure_after_the_scripts_are_kept_still_restores_the_old_ones() {
  let mut fixture = PicaFixture::new();
  let manifest = format!("{MANIFEST_DEMO}pkgmgr = pica\nscript_install = scripts/install\n");
  let env = ("scripts/.env", "PICA_KEEP_SCRIPT_INSTALL=true\n");
  let kept = "usr/lib/pica/scripts/demo/scripts/install";

  fixture
    .install_without_containers(
      &manifest,
      &[],
      &[],
      &[("scripts/install", "#!/bin/sh\necho old\n"), env],
    )
    .expect("the first install keeps the script");

  // The upgrade replaces the kept scripts and *then* fails on the payload.
  // The backup belongs to the transaction, not to this one step, so the old
  // script still comes back.
  let err = fixture
    .install_without_containers(
      &manifest,
      &[("aaa.conf", "overlay\n")],
      &[("zzz-escape", "/etc/passwd")],
      &[("scripts/install", "#!/bin/sh\necho new\n"), env],
    )
    .expect_err("the escaping symlink fails the payload");

  assert!(err.message.contains("escapes the target root"), "{}", err.message);
  assert_eq!(
    fs::read_to_string(fixture.path(kept)).expect("read the kept script"),
    "#!/bin/sh\necho old\n",
    "a failure after the scripts were kept must still put the old ones back"
  );
  assert_eq!(backup_stores(&fixture), Vec::<PathBuf>::new());
}

#[test]
fn a_successful_upgrade_drops_the_backup() {
  let mut fixture = PicaFixture::new();
  let manifest = format!("{MANIFEST_DEMO}pkgmgr = pica\nscript_install = scripts/install\n");
  let env = ("scripts/.env", "PICA_KEEP_SCRIPT_INSTALL=true\n");
  let kept = "usr/lib/pica/scripts/demo/scripts/install";

  fixture
    .install_without_containers(
      &manifest,
      &[],
      &[],
      &[("scripts/install", "#!/bin/sh\necho old\n"), env],
    )
    .expect("the first install keeps the script");
  fixture
    .install_without_containers(
      &manifest,
      &[],
      &[],
      &[("scripts/install", "#!/bin/sh\necho new\n"), env],
    )
    .expect("the upgrade keeps it too");

  assert_eq!(
    fs::read_to_string(fixture.path(kept)).expect("read the kept script"),
    "#!/bin/sh\necho new\n",
    "the upgrade replaces the old bytes"
  );
  assert_eq!(backup_stores(&fixture), Vec::<PathBuf>::new(), "and the old copy is cleaned up");
}

#[test]
fn an_upgrade_replaces_the_kept_scripts_as_a_set() {
  let mut fixture = PicaFixture::new();
  let env = ("scripts/.env", "PICA_KEEP_SCRIPT_INSTALL=true\nPICA_KEEP_SCRIPT_REMOVE=true\n");
  let both = format!(
    "{MANIFEST_DEMO}pkgmgr = pica\nscript_install = scripts/install\n\
     script_remove = scripts/remove\n"
  );
  let only_install = format!("{MANIFEST_DEMO}pkgmgr = pica\nscript_install = scripts/install\n");

  fixture
    .install_without_containers(
      &both,
      &[],
      &[],
      &[
        ("scripts/install", "#!/bin/sh\necho old\n"),
        ("scripts/remove", "#!/bin/sh\necho old\n"),
        env,
      ],
    )
    .expect("version 1 keeps both scripts");
  fixture
    .install_without_containers(
      &only_install,
      &[],
      &[],
      &[("scripts/install", "#!/bin/sh\necho new\n"), env],
    )
    .expect("version 2 keeps one");

  assert_eq!(
    fs::read_to_string(fixture.path("usr/lib/pica/scripts/demo/scripts/install"))
      .expect("read the kept script"),
    "#!/bin/sh\necho new\n"
  );
  assert!(
    !fixture.path("usr/lib/pica/scripts/demo/scripts/remove").exists(),
    "a script the new version does not keep is gone"
  );
}
