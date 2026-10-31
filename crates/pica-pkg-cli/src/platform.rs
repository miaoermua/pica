use crate::app::App;
use crate::i18n::Key;
use crate::i18n::tr;
use crate::pkgmgr::{PackageManager, PackageManagerKind, detect_kind, package_manager_for};
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

/// What this host could say about the package architectures it supports.
///
/// The sources disagree about both spelling and precision — the kernel answers
/// `aarch64` where opkg answers `aarch64_cortex-a53` — so the answer keeps its
/// source instead of flattening into a list: how much a name is worth decides
/// how strictly a package's `arch` may be measured against it.
///
/// An empty list never means "this host supports no architecture", so a
/// question that went unanswered is [`HostArches::Unknown`] and not an empty
/// [`HostArches::Backend`].
#[derive(Debug)]
pub(crate) enum HostArches {
  /// Names a package manager reported for itself, and which one it was.
  Backend { answered_by: PackageManagerKind, arches: Vec<String> },
  /// The kernel's own name, used when no package manager could answer.
  Kernel { uname: String },
  /// No name could be obtained at all.
  Unknown,
}

impl HostArches {
  /// What the host was able to say, for messages that have to explain an
  /// accept-or-refuse decision rather than just state it.
  #[must_use]
  pub(crate) fn describe(&self) -> String {
    match self {
      Self::Backend { answered_by, arches } => {
        tr(Key::ArchReportedBy, &[&answered_by.as_str(), &arches.join(" ")])
      }
      Self::Kernel { uname } => tr(Key::ArchFromUname, &[&uname]),
      Self::Unknown => tr(Key::ArchUnknown, &[]),
    }
  }
}

/// How a package's declared `arch` relates to what this host said about itself.
pub(crate) enum ArchVerdict {
  /// The backend named this arch itself.
  Backend,
  /// The kernel's name is exactly the declared one.
  Kernel,
  /// Only a weaker rule agreed: an alias (`amd64`/`x86_64`) or a
  /// sub-architecture of the kernel's family (`aarch64_cortex-a53` under
  /// `aarch64`).
  Related,
  /// Nothing could be compared, so the declaration stands unverified.
  Unverified,
  /// Nothing the host named is compatible with the declaration.
  Mismatch,
}

/// Asks this host which package architectures it accepts.
///
/// The backend in use is asked first: it measures the package's architecture
/// again when it installs, so its answer is the one that has to be agreed with.
/// When it cannot answer — the `pica` backend has no package manager at all —
/// whichever manager the host does have answers instead, because the
/// architectures a host accepts are a fact about the host rather than about who
/// installs the package.
pub(crate) fn detect_package_arches(app: &App) -> HostArches {
  let kind = app.backend();
  let detected = detect_kind();
  let other = if detected == kind { None } else { package_manager_for(detected) };

  reported_arches(kind, app.package_manager().ok())
    .or_else(|| reported_arches(detected, other.as_deref()))
    .unwrap_or_else(kernel_arches)
}

fn reported_arches(
  kind: PackageManagerKind,
  manager: Option<&dyn PackageManager>,
) -> Option<HostArches> {
  // An empty answer is not "this host accepts nothing" — it is a command that
  // failed, so it is passed over rather than believed.
  let arches = manager?.architectures();
  if arches.is_empty() {
    return None;
  }

  Some(HostArches::Backend { answered_by: kind, arches })
}

/// The kernel's name, for hosts where no package manager could be asked.
///
/// It is what keeps the `pica` backend usable for the packages that need it
/// most: almost every real package names a concrete arch, and without a list of
/// any kind every one of them would be refused for the sole reason that nothing
/// could be asked.
fn kernel_arches() -> HostArches {
  match run_command_text("uname", &["-m"]) {
    Ok(uname) if !uname.is_empty() => HostArches::Kernel { uname },
    _ => HostArches::Unknown,
  }
}

/// Decides whether `pkg_arch` may run on a host described by `host`.
///
/// A backend list is matched exactly: the backend checks the package's own
/// architecture again when it installs it, so accepting more than the backend
/// would only move the refusal somewhere later and more confusing.
#[must_use]
pub(crate) fn check_arch(pkg_arch: &str, host: &HostArches) -> ArchVerdict {
  match host {
    HostArches::Backend { arches, .. } => {
      if arches.iter().any(|arch| same_arch(pkg_arch, arch)) {
        ArchVerdict::Backend
      } else {
        ArchVerdict::Mismatch
      }
    }
    HostArches::Kernel { uname, .. } => {
      let uname = &with_byte_order(uname, cfg!(target_endian = "little"));
      if same_arch(pkg_arch, uname) {
        ArchVerdict::Kernel
      } else if arch_related(pkg_arch, uname) {
        ArchVerdict::Related
      } else {
        ArchVerdict::Mismatch
      }
    }
    HostArches::Unknown => ArchVerdict::Unverified,
  }
}

/// The kernel's name with the byte order it leaves out.
///
/// `uname -m` says `mips` (or `mips64`) for both byte orders, while every
/// package name spells the little-endian one `mipsel`. This binary is running,
/// so the host has the byte order it was built for.
fn with_byte_order(uname: &str, little_endian: bool) -> String {
  let uname = uname.trim();
  match uname {
    "mips" | "mips64" if little_endian => format!("{uname}el"),
    _ => uname.to_string(),
  }
}

fn same_arch(left: &str, right: &str) -> bool {
  left.trim().eq_ignore_ascii_case(right.trim())
}

/// Whether two names are two spellings of one CPU family, or a family and one
/// of its sub-architectures.
///
/// A sub-architecture is the family plus the variant (`aarch64_cortex-a53`),
/// while the kernel, opkg and apk each prefer their own spelling of the bare
/// family (`x86_64`/`amd64`, `aarch64`/`arm64`). The separator is what keeps
/// `mips` from matching `mipsel_24kc`, which are not interchangeable.
fn arch_related(left: &str, right: &str) -> bool {
  let left = arch_family(left);
  let right = arch_family(right);
  if left.is_empty() || right.is_empty() {
    return false;
  }

  left == right || is_sub_arch(&right, &left) || is_sub_arch(&left, &right)
}

/// The canonical spelling of a bare family name.
fn arch_family(value: &str) -> String {
  let lower = value.trim().to_ascii_lowercase();
  match lower.as_str() {
    "amd64" | "x86_64" => "x86_64".to_string(),
    "arm64" | "aarch64" => "aarch64".to_string(),
    _ => lower,
  }
}

/// Whether `specific` names a variant inside `family`.
fn is_sub_arch(family: &str, specific: &str) -> bool {
  specific.strip_prefix(family).is_some_and(|rest| rest.starts_with('_') || rest.starts_with('-'))
}

pub(crate) fn detect_luci_variant(app: &App) -> String {
  if let Ok(package_manager) = app.package_manager() {
    return luci_variant_of(package_manager);
  }

  // Which luci is installed is a fact about the host, not about who installs
  // the package. The `pica` backend has no package manager to ask, but that
  // says nothing about luci: the host may still run opkg or apk, and they are
  // then the only things that can answer.
  package_manager_for(detect_kind())
    .as_deref()
    .map_or_else(|| "unknown".to_string(), luci_variant_of)
}

fn luci_variant_of(package_manager: &dyn PackageManager) -> String {
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
  use super::{ArchVerdict, HostArches, arch_related, check_arch, normalize_uname};
  use crate::pkgmgr::PackageManagerKind;
  use pretty_assertions::assert_eq;

  fn backend(kind: PackageManagerKind, arches: &[&str]) -> HostArches {
    HostArches::Backend {
      answered_by: kind,
      arches: arches.iter().map(ToString::to_string).collect(),
    }
  }

  fn kernel(uname: &str) -> HostArches {
    HostArches::Kernel { uname: uname.to_string() }
  }

  #[test]
  fn a_little_endian_mips_kernel_is_matched_as_mipsel() {
    use super::with_byte_order;
    assert_eq!(with_byte_order("mips", true), "mipsel");
    assert_eq!(with_byte_order("mips64", true), "mips64el");
    assert_eq!(with_byte_order("mips", false), "mips");
    assert_eq!(with_byte_order("x86_64", true), "x86_64");
    // With the byte order supplied, a mipsel package is a sub-architecture of
    // the kernel's family, and a big-endian one is not.
    assert!(super::arch_related("mipsel_24kc", &with_byte_order("mips", true)));
    assert!(!super::arch_related("mips_24kc", &with_byte_order("mips", true)));
  }

  #[test]
  fn normalize_uname_maps_common_values() {
    assert_eq!(normalize_uname("x86_64"), "amd64");
    assert_eq!(normalize_uname("aarch64"), "arm64");
    assert_eq!(normalize_uname("mips"), "mips");
  }

  #[test]
  fn a_backend_list_is_matched_exactly() {
    // Whatever the backend listed is what the backend will measure the package
    // against itself, so agreeing with anything else would only move the
    // refusal to a later, less explicable place.
    let host = backend(PackageManagerKind::Apk, &["x86_64", "aarch64_cortex-a53"]);

    assert!(matches!(check_arch("x86_64", &host), ArchVerdict::Backend));
    assert!(matches!(check_arch("X86_64", &host), ArchVerdict::Backend));
    assert!(matches!(check_arch("aarch64_cortex-a53", &host), ArchVerdict::Backend));

    assert!(matches!(check_arch("aarch64", &host), ArchVerdict::Mismatch));
    assert!(matches!(check_arch("amd64", &host), ArchVerdict::Mismatch));
    assert!(matches!(check_arch("aarch64_cortex-a55", &host), ArchVerdict::Mismatch));
  }

  #[test]
  fn a_kernel_name_covers_its_own_family() {
    // With no package manager anywhere on the host the kernel's name is all
    // there is — and it names a CPU family, not the sub-architecture a package
    // may have been built for.
    let host = kernel("aarch64");

    assert!(matches!(check_arch("aarch64", &host), ArchVerdict::Kernel));
    assert!(matches!(check_arch("aarch64_cortex-a53", &host), ArchVerdict::Related));
    assert!(matches!(check_arch("arm64", &host), ArchVerdict::Related));

    assert!(matches!(check_arch("x86_64", &host), ArchVerdict::Mismatch));
    assert!(matches!(check_arch("mipsel_24kc", &host), ArchVerdict::Mismatch));
  }

  #[test]
  fn an_unanswered_host_accepts_the_declaration_unverified() {
    // Reaching here means no package manager reported a list and the kernel
    // could not be read either. That is "unknown", which is not the same as
    // "incompatible", and the gate says so instead of refusing every concrete
    // arch for the sole reason that nothing could be asked.
    assert!(matches!(
      check_arch("aarch64_cortex-a53", &HostArches::Unknown),
      ArchVerdict::Unverified
    ));
  }

  #[test]
  fn a_family_does_not_swallow_a_neighbouring_cpu() {
    assert!(!arch_related("mips", "mipsel_24kc"), "endianness is not a sub-architecture");
    assert!(!arch_related("x86_64", "i486"));
    assert!(!arch_related("arm", "armv7"));

    assert!(arch_related("mipsel", "mipsel_24kc"));
    assert!(arch_related("x86_64", "amd64"));
    assert!(arch_related("aarch64_cortex-a53", "aarch64"));
  }

  #[test]
  fn the_host_description_names_where_the_answer_came_from() {
    assert_eq!(backend(PackageManagerKind::Opkg, &["x86_64"]).describe(), "opkg reports x86_64");
    assert_eq!(
      backend(PackageManagerKind::Apk, &["aarch64_cortex-a53", "x86_64"]).describe(),
      "apk reports aarch64_cortex-a53 x86_64"
    );
    assert_eq!(
      kernel("aarch64").describe(),
      "no package manager reported an arch list; uname -m reports aarch64"
    );
    assert_eq!(
      HostArches::Unknown.describe(),
      "no package manager reported an arch list and uname -m could not be read"
    );
  }
}
