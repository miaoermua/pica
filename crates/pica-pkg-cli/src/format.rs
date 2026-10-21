//! Formatting shared by the screens that show a size.

/// A byte count the way a package manager prints one: `512 B`, `1.5 KiB`,
/// `74.2 MiB`, `2.0 GiB`.
///
/// One decimal from KiB up, none below it, because bytes are exact and the
/// larger units are not. Binary multiples: a package that says 1 MiB means
/// 1048576 bytes. GiB is the largest unit; a larger count stays in GiB rather
/// than inventing a unit this CLI does not print.
#[allow(clippy::cast_precision_loss, reason = "a byte count is far below 2^52")]
pub(crate) fn bytes(bytes: u64) -> String {
  const UNITS: [(&str, u64); 4] =
    [("GiB", 1024 * 1024 * 1024), ("MiB", 1024 * 1024), ("KiB", 1024), ("", 1)];

  for (unit, factor) in UNITS {
    if bytes >= factor {
      let value = bytes as f64 / factor as f64;
      return if unit.is_empty() { format!("{bytes} B") } else { format!("{value:.1} {unit}") };
    }
  }

  format!("{bytes} B")
}

/// `1 file`, `2 files`.
pub(crate) fn files(count: usize) -> String {
  format!("{count} file{}", if count == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
  use super::*;
  use pretty_assertions::assert_eq;

  #[test]
  fn sizes_use_binary_units_and_round_to_one_decimal() {
    assert_eq!(bytes(0), "0 B");
    assert_eq!(bytes(512), "512 B");
    assert_eq!(bytes(1024), "1.0 KiB");
    assert_eq!(bytes(1536), "1.5 KiB");
    assert_eq!(bytes(74 * 1024 * 1024 + 200 * 1024), "74.2 MiB");
    assert_eq!(bytes(2 * 1024 * 1024 * 1024), "2.0 GiB");
    // GiB is the largest unit: a larger count stays in GiB.
    assert_eq!(bytes(2 * 1024 * 1024 * 1024 * 1024), "2048.0 GiB");
    // The largest unit that fits: 1000 KiB is still under 1 MiB, so it stays
    // in KiB instead of being rounded up into a unit it does not reach.
    assert_eq!(bytes(1000 * 1024), "1000.0 KiB");
  }

  #[test]
  fn a_single_file_is_not_pluralised() {
    assert_eq!(files(0), "0 files");
    assert_eq!(files(1), "1 file");
    assert_eq!(files(2), "2 files");
  }
}
