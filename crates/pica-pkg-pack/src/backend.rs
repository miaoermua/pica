use pica_pkg_core::error::{PicaError, PicaResult};
use pica_pkg_core::manifest::split_targets;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum BuildBackend {
  Opkg,
  Apk,
  None,
}

impl BuildBackend {
  pub(crate) fn parse(value: &str) -> PicaResult<Self> {
    match value.trim().to_ascii_lowercase().as_str() {
      "opkg" => Ok(Self::Opkg),
      "apk" => Ok(Self::Apk),
      "none" => Ok(Self::None),
      other => Err(PicaError::msg(format!("unsupported package manager: {other}"))),
    }
  }

  pub(crate) fn as_str(self) -> &'static str {
    match self {
      Self::Opkg => "opkg",
      Self::Apk => "apk",
      Self::None => "none",
    }
  }

  /// Subdirectory of `--outdir` holding this backend's artifacts.
  ///
  /// Every backend gets its own directory. Deciding the backend from a file
  /// extension is what let a `none` build pick up `.apk` inputs, so the layout
  /// now states the backend instead of implying it.
  pub(crate) fn output_subdir(self) -> &'static str {
    self.as_str()
  }

  pub(crate) fn package_extension(self) -> Option<&'static str> {
    match self {
      Self::Opkg => Some(".ipk"),
      Self::Apk => Some(".apk"),
      Self::None => None,
    }
  }

  /// `none` ships no system packages: it delegates nothing to a package manager.
  pub(crate) fn is_native(self) -> bool {
    !matches!(self, Self::None)
  }
}

pub(crate) fn parse_targets(flag: Option<&str>, manifest_targets: &[String]) -> PicaResult<Vec<BuildBackend>> {
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
  use super::{parse_targets, BuildBackend};

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
    assert_eq!(BuildBackend::Apk.package_extension(), Some(".apk"));
    assert_eq!(BuildBackend::Opkg.package_extension(), Some(".ipk"));
    assert_eq!(BuildBackend::None.package_extension(), None);
    assert_eq!(BuildBackend::Opkg.output_subdir(), "opkg");
    assert_eq!(BuildBackend::Apk.output_subdir(), "apk");
    assert_eq!(BuildBackend::None.output_subdir(), "none");
    assert!(!BuildBackend::None.is_native());
    assert!(BuildBackend::Apk.is_native());
  }

  #[test]
  fn rejects_unknown_backend() {
    assert!(parse_targets(Some("pacman"), &[]).is_err());
  }
}
