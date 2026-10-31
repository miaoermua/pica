//! Checks that refuse an artifact before anything is written.

use super::*;
use pretty_assertions::assert_eq;

#[test]
fn an_unreadable_index_does_not_stop_an_artifact_that_requires_nothing() {
  let work = tempfile::tempdir().expect("temp dir");
  let mut app = app_with_index(work.path(), false);

  let deps = declarations(&[], &[], &["opt-app?"]);
  ensure_index(&mut app, &deps).expect("optional declarations do not need the index");
}

#[test]
fn an_unreadable_index_still_stops_anything_required() {
  let work = tempfile::tempdir().expect("temp dir");

  for deps in
    [declarations(&[], &["base-dep"], &["opt-app?"]), declarations(&[], &[], &["app-dep"])]
  {
    let mut app = app_with_index(work.path(), false);
    let err = ensure_index(&mut app, &deps).expect_err("a required dependency needs the index");
    assert_eq!(err.code, E_INDEX_UNAVAILABLE, "{err:?}");
  }
}

#[test]
fn a_required_kernel_module_counts_as_requiring_the_index() {
  // The precheck has three groups, and the source decision only looks at two.
  // A guard that forgot `kmod` would let a module-only artifact through on an
  // index that never answered.
  let work = tempfile::tempdir().expect("temp dir");
  let mut app = app_with_index(work.path(), false);

  let deps = declarations(&["kmod-tun"], &[], &["opt-app?"]);
  let err = ensure_index(&mut app, &deps).expect_err("a required module needs the index");
  assert_eq!(err.code, E_INDEX_UNAVAILABLE, "{err:?}");
}

#[test]
fn a_readable_index_is_asked_for_optional_and_required_alike() {
  // The downgrade is about a failure, not about skipping the refresh: an index
  // that answers must still be asked, so the optional names get a real status
  // rather than "unknown".
  for deps in [declarations(&[], &[], &["opt-app?"]), declarations(&[], &["base-dep"], &[])] {
    let work = tempfile::tempdir().expect("temp dir");
    let mut app = app_with_index(work.path(), true);
    ensure_index(&mut app, &deps).expect("a readable index never fails the precheck");
  }
}

#[test]
fn a_name_that_would_leave_the_scripts_directory_is_refused_before_anything_is_written() {
  let mut fixture = PicaFixture::new();
  let manifest =
    format!("{MANIFEST_DEMO}pkgmgr = pica\n").replace("pkgname = demo", "pkgname = ../../evil");

  let err = fixture
    .install_without_containers(&manifest, &[("etc/demo.conf", "overlay\n")], &[], &[])
    .expect_err("the name must be refused");

  assert_eq!(err.code, E_MANIFEST_INVALID);
  assert!(err.message.contains("../../evil"), "{}", err.message);
  assert!(!fixture.path("etc/demo.conf").exists(), "nothing may be laid down");
}

#[test]
fn an_appname_a_selector_could_not_reach_is_refused() {
  let mut fixture = PicaFixture::new();
  let manifest =
    format!("{MANIFEST_DEMO}pkgmgr = pica\n").replace("appname = demo", "appname = demo@alice");

  let err = fixture
    .install_without_containers(&manifest, &[("etc/demo.conf", "overlay\n")], &[], &[])
    .expect_err("the name must be refused");

  assert_eq!(err.code, E_MANIFEST_INVALID);
  assert!(err.message.contains("demo@alice"), "{}", err.message);
}

#[test]
fn a_pica_artifact_must_not_carry_containers() {
  // pica does not read container formats: it could neither install one nor
  // account for the files inside it, so an artifact that carries `pkgs/` is
  // refused before anything is written rather than half-installed.
  let mut fixture = PicaFixture::new();

  let err = fixture
    .install(
      &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
      &[("etc/demo.conf", "overlay\n")],
      &[],
      &[],
    )
    .expect_err("a pica artifact must not ship containers");

  assert!(err.message.contains("must not carry pkgs/"), "{}", err.message);
  assert!(
    err.message.contains("targets = apk"),
    "the error must offer the way forward: {}",
    err.message
  );
  assert!(
    !fixture.path("etc/demo.conf").exists(),
    "nothing may be laid down when the artifact is refused"
  );
  assert!(
    db_record(&fixture.app, "demo").is_none(),
    "a refused install must leave no database record"
  );
}

#[test]
fn pica_rejects_a_package_that_declares_dependencies() {
  let mut fixture = PicaFixture::new();

  let err = fixture
    .install(
      &format!("{MANIFEST_DEMO}pkgmgr = pica\nbase = luci-base\n"),
      &[("etc/demo.conf", "overlay\n")],
      &[],
      &[],
    )
    .expect_err("D3: pica cannot resolve dependencies");

  assert!(err.message.contains("cannot resolve dependencies"), "{}", err.message);
  assert!(
    err.message.contains("pkgmgr=opkg"),
    "the message must suggest a backend that can install them: {}",
    err.message
  );
  assert_eq!(err.code, E_PACKAGE_INVALID);
  assert!(!fixture.path("etc/demo.conf").exists(), "validation runs before anything is written");
  assert!(db_record(&fixture.app, "demo").is_none());
}

#[test]
fn every_declared_source_availability_is_installed_and_recorded() {
  for value in ["open", "partial", "closed"] {
    let mut fixture = PicaFixture::new();
    fixture
      .install_without_containers(
        &format!("{MANIFEST_DEMO}pkgmgr = pica\nsource_availability = {value}\n"),
        &[("etc/demo.conf", "overlay\n")],
        &[],
        &[],
      )
      .unwrap_or_else(|err| panic!("{value} must install: {}", err.message));

    let record = db_record(&fixture.app, "demo").expect("the install is recorded");
    assert_eq!(
      record["manifest"]["source_availability"].as_str(),
      Some(value),
      "the recorded manifest keeps the value the artifact declared"
    );
  }
}

#[test]
fn an_unknown_source_availability_is_refused_before_anything_is_written() {
  let mut fixture = PicaFixture::new();

  let err = fixture
    .install_without_containers(
      &format!("{MANIFEST_DEMO}pkgmgr = pica\nsource_availability = public\n"),
      &[("etc/demo.conf", "overlay\n")],
      &[],
      &[],
    )
    .expect_err("`public` is not one of the declared values");

  assert_eq!(err.code, E_CONFIG_INVALID);
  assert!(err.message.contains("invalid source_availability"), "{}", err.message);
  assert!(err.message.contains("open, partial, closed"), "{}", err.message);
  assert!(!fixture.path("etc/demo.conf").exists(), "the precheck runs before any deployment");
  assert!(db_record(&fixture.app, "demo").is_none());
}

#[test]
fn a_package_that_declares_no_source_availability_is_still_installed() {
  for declaration in ["", "source_availability =\n"] {
    let mut fixture = PicaFixture::new();
    fixture
      .install_without_containers(
        &format!("{MANIFEST_DEMO}pkgmgr = pica\n{declaration}"),
        &[("etc/demo.conf", "overlay\n")],
        &[],
        &[],
      )
      .unwrap_or_else(|err| panic!("{declaration:?} must be accepted: {}", err.message));

    let record = db_record(&fixture.app, "demo").expect("the install is recorded");
    assert_eq!(
      record["manifest"]["source_availability"].as_str(),
      Some(""),
      "an undeclared value is stored as empty, the same way the other optional fields are"
    );
  }
}

#[test]
fn a_container_in_a_pica_artifact_is_refused_before_anything_is_written() {
  // pica never opens a container, so it cannot tell a broken one from a sound
  // one — the artifact is refused on its shape alone.
  let mut fixture = PicaFixture::new();
  let broken = fixture.work_path.join("broken.ipk");
  write_file(&broken, "this is not a container\n");

  let err = install_artifact(
    &fixture.work_path,
    &mut fixture.app,
    &fixture.target,
    &format!("{MANIFEST_DEMO}pkgmgr = pica\n"),
    &[("etc/demo.conf", "overlay\n")],
    &[],
    &[("broken.ipk", broken)],
    &[],
  )
  .expect_err("step 5 fails");

  assert!(err.message.contains("must not carry pkgs/"), "{}", err.message);
  assert!(
    err.message.contains("targets = opkg") && err.message.contains("targets = apk"),
    "the error must name the backends that do install it: {}",
    err.message
  );
  assert!(
    !fixture.path("etc/demo.conf").exists(),
    "the payload must not run after the container step failed"
  );
  assert!(db_record(&fixture.app, "demo").is_none(), "a failed install leaves no record");
}

#[test]
fn a_concrete_arch_is_measured_against_the_kernel_on_the_pica_backend() {
  let mut fixture = PicaFixture::new();

  // The `pica` backend has no package manager to ask, so the kernel's name is
  // the only thing left to compare against. Before the fallback existed the
  // list came back empty and every package naming a concrete arch was refused
  // as unsupported, so the message is what this test is really watching.
  let manifest = format!("{}pkgmgr = pica\n", MANIFEST_DEMO.replace("arch = all", "arch = s390x"));
  let err = fixture.install(&manifest, &[], &[], &[]).expect_err("the arch is foreign");

  assert!(err.message.contains("unsupported arch: pkg=s390x"), "{}", err.message);
  assert!(
    err.message.contains("uname -m reports"),
    "the host has to say where its answer came from, not just that it has none: {}",
    err.message
  );
  assert!(
    !fixture.path("usr/lib/demo/helper").exists(),
    "a rejected arch must not deploy the payload first"
  );
  assert!(db_record(&fixture.app, "demo").is_none(), "nothing was installed, nothing is recorded");
}

#[test]
fn a_container_is_named_as_one_instead_of_as_a_broken_archive() {
  let work = tempfile::tempdir().expect("temp dir");
  let apk = work.path().join("demo.apk");
  fs::write(&apk, "an apk is not a tar every tar can read\n").expect("write apk");

  let mut app = test_app(work.path());
  let target = TargetRoot::new(work.path().join("target-root"));
  let err = pkgfile_into(&mut app, &apk, None, EntrySource::Local, IndexSizes::default(), &target)
    .expect_err("-U does not install containers");

  assert!(err.message.contains("is an apk package"), "{}", err.message);
  assert!(err.message.contains("apk add"), "{}", err.message);
  assert!(
    !err.message.contains("invalid magic"),
    "the reader should not have to decode a tar error: {}",
    err.message
  );
}
