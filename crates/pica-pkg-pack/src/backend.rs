use pica_pkg_core::error::{PicaError, PicaResult};

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

  #[allow(dead_code)]
  pub(crate) fn input_extension(self) -> Option<&'static str> {
    match self {
      Self::Opkg => Some(".ipk"),
      Self::Apk => Some(".apk"),
      Self::None => None,
    }
  }

  #[allow(dead_code)]
  pub(crate) fn output_suffix(self) -> &'static str {
    match self {
      Self::Apk => "-apk",
      Self::Opkg | Self::None => "",
    }
  }
}

pub(crate) fn parse_targets(value: Option<&str>, manifest_value: &str) -> PicaResult<Vec<BuildBackend>> {
  let raw = value.unwrap_or(if manifest_value.is_empty() { "opkg" } else { manifest_value });
  let mut result = Vec::new();
  for item in raw.split(',') {
    let backend = BuildBackend::parse(item)?;
    if !result.contains(&backend) {
      result.push(backend);
    }
  }
  if result.is_empty() {
    return Err(PicaError::msg("at least one package manager target is required"));
  }
  Ok(result)
}

#[cfg(test)]
mod tests {
  use super::{parse_targets, BuildBackend};

  #[test]
  fn parses_deduplicated_targets() {
    assert_eq!(parse_targets(Some("apk,opkg,apk"), "").unwrap(), vec![BuildBackend::Apk, BuildBackend::Opkg]);
    assert_eq!(parse_targets(None, "").unwrap(), vec![BuildBackend::Opkg]);
    assert_eq!(BuildBackend::Apk.output_suffix(), "-apk");
  }
}
