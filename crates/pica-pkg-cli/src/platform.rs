use crate::system::{opkg_is_installed, run_command_text};
use std::fs;
use std::process::Command;

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

pub(crate) fn detect_opkg_arches() -> Vec<String> {
  let Ok(output) = Command::new("opkg").arg("print-architecture").output() else {
    return Vec::new();
  };

  let text = String::from_utf8_lossy(&output.stdout);
  let mut out = Vec::new();
  for line in text.lines() {
    let mut parts = line.split_whitespace();
    let _ = parts.next();
    if let Some(arch) = parts.next() {
      out.push(arch.to_string());
    }
  }
  out
}

pub(crate) fn detect_luci_variant() -> String {
  if opkg_is_installed("luci") {
    return "lua1".to_string();
  }
  if opkg_is_installed("luci2") {
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
