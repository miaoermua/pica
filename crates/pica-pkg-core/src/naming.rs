//! What `pkgname` and `appname` may contain.
//!
//! A `pkgname` becomes part of paths on the device, such as
//! `/usr/lib/pica/scripts/<pkgname>/`, and of the artifact's filename. An
//! `appname` is what a selector matches. The packer, the index and the installer
//! all apply these rules, so a name one of them accepts is never refused by
//! another.

const MAX_NAME_LEN: usize = 128;

/// Whether `name` is a valid `pkgname`: an ASCII letter or digit, followed by
/// letters, digits and `. _ + - @`. `@` marks who made the package.
#[must_use]
pub fn is_valid_pkgname(name: &str) -> bool {
  is_valid(name, |ch| ch == '@')
}

/// Whether `name` is a valid `appname`: the `pkgname` rule without `@`, which a
/// selector reads as a branch separator.
#[must_use]
pub fn is_valid_appname(name: &str) -> bool {
  is_valid(name, |_| false)
}

fn is_valid(name: &str, also: impl Fn(char) -> bool) -> bool {
  let mut chars = name.chars();
  name.len() <= MAX_NAME_LEN
    && chars.next().is_some_and(|first| first.is_ascii_alphanumeric())
    && chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '+' | '-') || also(ch))
}

#[cfg(test)]
mod tests {
  use super::{MAX_NAME_LEN, is_valid_appname, is_valid_pkgname};

  #[test]
  fn names_a_source_publishes_are_accepted() {
    for name in ["demo", "demo-bin", "luci-app-demo", "libstdc++6", "a.b_c", "7zip"] {
      assert!(is_valid_pkgname(name), "{name}");
      assert!(is_valid_appname(name), "{name}");
    }
  }

  #[test]
  fn only_a_pkgname_may_say_who_made_it() {
    assert!(is_valid_pkgname("ddns-go@immortalwrt"));
    assert!(!is_valid_appname("ddns-go@immortalwrt"));
  }

  #[test]
  fn a_name_that_could_leave_its_directory_is_refused() {
    for name in ["", ".", "..", "../evil", "a/b", "a\\b", "/etc/passwd", ".hidden", "-rf"] {
      assert!(!is_valid_pkgname(name), "{name:?}");
      assert!(!is_valid_appname(name), "{name:?}");
    }
  }

  #[test]
  fn whitespace_control_and_non_ascii_characters_are_refused() {
    for name in ["a b", "a\tb", "a\nb", "a\0b", "包名", "a(b)", "a:b"] {
      assert!(!is_valid_pkgname(name), "{name:?}");
    }
  }

  #[test]
  fn a_name_has_a_length_limit() {
    assert!(is_valid_pkgname(&"a".repeat(MAX_NAME_LEN)));
    assert!(!is_valid_pkgname(&"a".repeat(MAX_NAME_LEN + 1)));
  }
}
