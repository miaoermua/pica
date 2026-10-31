use pica_pkg_core::error::{PicaError, PicaResult};
use pica_pkg_core::manifest::split_targets;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum BuildBackend {
  Opkg,
  Apk,
  Pica,
}

impl BuildBackend {
  pub(crate) fn parse(value: &str) -> PicaResult<Self> {
    match value.trim().to_ascii_lowercase().as_str() {
      "opkg" => Ok(Self::Opkg),
      "apk" => Ok(Self::Apk),
      "pica" => Ok(Self::Pica),
      other => Err(PicaError::msg(format!("unsupported package manager: {other}"))),
    }
  }

  pub(crate) fn as_str(self) -> &'static str {
    match self {
      Self::Opkg => "opkg",
      Self::Apk => "apk",
      Self::Pica => "pica",
    }
  }

  /// Subdirectory of `--outdir` holding this backend's artifacts.
  ///
  /// Every backend gets its own directory. Deciding the backend from a file
  /// extension is what let a `pica` build pick up `.apk` inputs, so the layout
  /// now states the backend instead of implying it.
  pub(crate) fn output_subdir(self) -> &'static str {
    self.as_str()
  }

  /// Container formats `pkgs/<backend>/` may hold.
  ///
  /// `opkg` and `apk` accept exactly their own format, so a stray `.apk` under
  /// `pkgs/opkg/` is reported instead of packed. A `pica` artifact carries no
  /// container at all, so the list is empty: nothing may be put there.
  pub(crate) fn package_extensions(self) -> &'static [&'static str] {
    match self {
      Self::Opkg => &[".ipk"],
      Self::Apk => &[".apk"],
      Self::Pica => &[],
    }
  }

  /// Whether an artifact of this backend ships `pkgs/` containers.
  ///
  /// Only the backend that installs them does: `opkg`/`apk` hand the containers
  /// to the system package manager on the target, which keeps that backend's
  /// database and dependency solver in charge. A `pica` artifact ships `root/`
  /// and nothing else — pica deploys files, it does not read container formats.
  pub(crate) fn expects_containers(self) -> bool {
    match self {
      Self::Opkg | Self::Apk => true,
      Self::Pica => false,
    }
  }
}

pub(crate) fn parse_targets(
  flag: Option<&str>,
  manifest_targets: &[String],
) -> PicaResult<Vec<BuildBackend>> {
  let declared = match flag {
    Some(value) => split_targets(&[value.to_string()]),
    None => split_targets(manifest_targets),
  };

  let source = if declared.is_empty() { vec!["opkg".to_string()] } else { declared };

  let mut result = Vec::new();
  for item in &source {
    let backend = BuildBackend::parse(item)?;
    if !result.contains(&backend) {
      result.push(backend);
    }
  }
  Ok(result)
}

#[cfg(test)]
mod tests {
  use super::{BuildBackend, parse_targets};

  #[test]
  fn parses_deduplicated_targets() {
    assert_eq!(
      parse_targets(Some("apk,opkg,apk"), &[]).unwrap(),
      vec![BuildBackend::Apk, BuildBackend::Opkg]
    );
    assert_eq!(parse_targets(None, &[]).unwrap(), vec![BuildBackend::Opkg]);
  }

  #[test]
  fn repeated_manifest_targets_are_all_honoured() {
    let targets = vec!["opkg".to_string(), "apk".to_string()];
    assert_eq!(
      parse_targets(None, &targets).unwrap(),
      vec![BuildBackend::Opkg, BuildBackend::Apk],
      "the repeatable form must not silently drop a backend"
    );
  }

  #[test]
  fn backend_drives_extension_and_output_dir() {
    assert_eq!(BuildBackend::Apk.package_extensions(), [".apk"]);
    assert_eq!(BuildBackend::Opkg.package_extensions(), [".ipk"]);
    assert_eq!(
      BuildBackend::Pica.package_extensions(),
      [] as [&str; 0],
      "pica ships no container, so no format is accepted under pkgs/pica/"
    );
    assert_eq!(BuildBackend::Opkg.output_subdir(), "opkg");
    assert_eq!(BuildBackend::Apk.output_subdir(), "apk");
    assert_eq!(BuildBackend::Pica.output_subdir(), "pica");
  }

  #[test]
  fn only_the_backends_that_install_containers_ship_them() {
    for backend in [BuildBackend::Opkg, BuildBackend::Apk] {
      assert!(backend.expects_containers(), "{backend:?}");
      assert!(!backend.package_extensions().is_empty(), "{backend:?}");
    }
    assert!(!BuildBackend::Pica.expects_containers());
    assert_eq!(BuildBackend::Pica.package_extensions(), [] as [&str; 0]);
  }

  #[test]
  fn rejects_unknown_backend() {
    assert!(parse_targets(Some("pacman"), &[]).is_err());
  }

  #[test]
  fn the_retired_none_target_is_rejected() {
    let err = parse_targets(Some("none"), &[]).expect_err("`none` was replaced by `pica`");
    assert!(err.to_string().contains("unsupported package manager"), "{err}");
  }
}
