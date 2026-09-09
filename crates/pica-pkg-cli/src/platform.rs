use crate::app::App;
use crate::system::run_command_text;
use std::fs;

pub(crate) fn detect_platform() -> String {
  let uname = run_command_text("uname", &["-m"]).unwrap_or_else(|_| "unknown".to_string());
  normalize_uname(&uname)
}

pub(crate) fn detect_os() -> String {
  if let Ok(openwrt_release) = fs::read_to_string("/etc/openwrt_release") {
    if openwrt_release.contains("DISTRIB_ID='OpenWrt'")
      || openwrt_release.contains("DISTRIB_ID=\"OpenWrt\"")
      || openwrt_release.contains("DISTRIB_ID=OpenWrt")
    {
      return "openwrt".to_string();
    }
  }

  "linux".to_string()
}

pub(crate) fn normalize_uname(value: &str) -> String {
  match value {
    "x86_64" => "amd64".to_string(),
    "aarch64" => "arm64".to_string(),
    other => other.to_string(),
  }
}

pub(crate) fn detect_package_arches(app: &App) -> Vec<String> {
  app.package_manager().map(crate::pkgmgr::PackageManager::architectures).unwrap_or_default()
}

pub(crate) fn detect_luci_variant(app: &App) -> String {
  let Ok(package_manager) = app.package_manager() else {
    return "unknown".to_string();
  };
  if package_manager.is_installed("luci") {
    return "lua1".to_string();
  }
  if package_manager.is_installed("luci2") {
    return "js2".to_string();
  }
  "unknown".to_string()
}

#[cfg(test)]
mod tests {
  use super::normalize_uname;
  use pretty_assertions::assert_eq;

  #[test]
  fn normalize_uname_maps_common_values() {
    assert_eq!(normalize_uname("x86_64"), "amd64");
    assert_eq!(normalize_uname("aarch64"), "arm64");
    assert_eq!(normalize_uname("mips"), "mips");
  }
}
