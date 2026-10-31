//! Names the protocol fixes: where an artifact keeps its parts, where a device
//! keeps what it installs, and which backends exist. See docs/standard.md.
//!
//! Each value is defined here and nowhere else, so a change to one of them is a
//! change to the protocol and shows up as exactly one diff.

/// Extension of every Pica artifact.
pub const PKG_EXTENSION: &str = ".pkg.tar.gz";

/// Backends a package can belong to.
pub const SUPPORTED_PKGMGRS: [&str; 3] = ["opkg", "apk", "pica"];

/// The manifest file of a pkgdir or artifact.
pub const MANIFEST: &str = "manifest";
/// Lifecycle scripts of a pkgdir or artifact.
pub const SCRIPTS: &str = "scripts";
/// Files mirrored onto the target root.
pub const ROOT: &str = "root";
/// Containers handed to the system package manager.
pub const PKGS: &str = "pkgs";
/// The optional license file of a pkgdir or artifact.
pub const LICENSE: &str = "LICENSE";

/// Where kept lifecycle scripts live on the target system.
pub const SCRIPTS_DIR: &str = "/usr/lib/pica/scripts";
/// Where package env files live on the target system.
pub const ENV_DIR: &str = "/etc/pica/env.d";

/// Language pack code used when the configuration names none. It selects
/// `luci-i18n-*` packages and has nothing to do with the language Pica speaks.
pub const DEFAULT_LUCI_I18N: &str = "zh-cn";
