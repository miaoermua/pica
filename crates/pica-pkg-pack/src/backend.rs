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
  /// `pkgs/opkg/` is reported instead of packed. `pica` unpacks the containers
  /// itself and understands both formats, so it keeps both.
  pub(crate) fn package_extensions(self) -> &'static [&'static str] {
    match self {
      Self::Opkg => &[".ipk"],
      Self::Apk => &[".apk"],
      Self::Pica => &[".ipk", ".apk"],
    }
  }

  /// Whether an artifact of this backend ships `pkgs/` containers.
  ///
  /// All three do, which is why the answer is uniform: `opkg`/`apk` hand the
  /// containers to the system package manager on the target, and `pica` unpacks
  /// them itself. The accessor states the layout rule in one place, so a backend
  /// that owns its files *without* shipping any container has somewhere to say
  /// so — and so call sites read as "does this rebuild include `pkgs/`".
  pub(crate) fn expects_containers(self) -> bool {
    match self {
      Self::Opkg | Self::Apk | Self::Pica => true,
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
      [".ipk", ".apk"],
      "pica unpacks containers itself and keeps both formats"
    );
    assert_eq!(BuildBackend::Opkg.output_subdir(), "opkg");
    assert_eq!(BuildBackend::Apk.output_subdir(), "apk");
    assert_eq!(BuildBackend::Pica.output_subdir(), "pica");
  }

  #[test]
  fn every_backend_ships_containers() {
    for backend in [BuildBackend::Opkg, BuildBackend::Apk, BuildBackend::Pica] {
      assert!(backend.expects_containers(), "{backend:?}");
      assert!(!backend.package_extensions().is_empty(), "{backend:?}");
    }
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
