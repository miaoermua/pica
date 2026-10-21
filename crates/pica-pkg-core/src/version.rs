use std::cmp::Ordering;

#[must_use]
pub fn ver_ge(a: &str, b: &str) -> bool {
  compare_dot_numeric(a, b).is_some_and(|ord| ord != Ordering::Less)
}

#[must_use]
pub fn pkgver_cmp_key(pkgver: &str, pkgrel: &str) -> String {
  if pkgrel.is_empty() { pkgver.to_string() } else { format!("{pkgver}-{pkgrel}") }
}

/// Total order over `pkgver-pkgrel` keys. Both sides must be non-empty; callers
/// that need the "unset upper bound means no constraint" convention use
/// [`pkgver_ge`] / [`pkgver_gt`] instead.
#[must_use]
pub fn pkgver_cmp(a: &str, b: &str) -> Ordering {
  let (a_ver, a_rel) = split_pkgver_rel(a);
  let (b_ver, b_rel) = split_pkgver_rel(b);

  let order = if is_dot_numeric(&a_ver) && is_dot_numeric(&b_ver) {
    compare_dot_numeric(&a_ver, &b_ver).unwrap_or(Ordering::Equal)
  } else {
    compare_mixed_version(&a_ver, &b_ver)
  };

  if order == Ordering::Equal { compare_rel(&a_rel, &b_rel) } else { order }
}

#[must_use]
pub fn pkgver_ge(a: &str, b: &str) -> bool {
  if b.is_empty() {
    return true;
  }
  if a.is_empty() {
    return false;
  }

  pkgver_cmp(a, b) != Ordering::Less
}

/// Strictly newer than `b`. Unlike [`pkgver_ge`], an empty `b` is not treated as
/// "no constraint": selecting a candidate must prove it is an upgrade.
#[must_use]
pub fn pkgver_gt(a: &str, b: &str) -> bool {
  if a.is_empty() || b.is_empty() {
    return false;
  }

  pkgver_cmp(a, b) == Ordering::Greater
}

fn compare_mixed_version(a: &str, b: &str) -> Ordering {
  let a_tokens = tokenize_version(a);
  let b_tokens = tokenize_version(b);

  let len = a_tokens.len().max(b_tokens.len());
  for index in 0..len {
    let left = a_tokens.get(index).copied().unwrap_or(Token::Num(0));
    let right = b_tokens.get(index).copied().unwrap_or(Token::Num(0));
    let ord = compare_token(left, right);
    if ord != Ordering::Equal {
      return ord;
    }
  }

  Ordering::Equal
}

#[derive(Clone, Copy)]
enum Token<'a> {
  Num(u64),
  Text(&'a str),
}

fn tokenize_version(input: &str) -> Vec<Token<'_>> {
  let mut out = Vec::new();
  let mut it = input.char_indices().peekable();

  while let Some((start, ch)) = it.next() {
    if !ch.is_ascii_alphanumeric() {
      continue;
    }

    let is_digit = ch.is_ascii_digit();
    let mut end = start + ch.len_utf8();

    while let Some(&(next_idx, next_ch)) = it.peek() {
      if !next_ch.is_ascii_alphanumeric() || next_ch.is_ascii_digit() != is_digit {
        break;
      }
      end = next_idx + next_ch.len_utf8();
      it.next();
    }

    let segment = &input[start..end];

    if is_digit {
      let value = segment.parse::<u64>().unwrap_or(0);
      out.push(Token::Num(value));
    } else {
      out.push(Token::Text(segment));
    }
  }

  out
}

fn compare_token(left: Token<'_>, right: Token<'_>) -> Ordering {
  match (left, right) {
    (Token::Num(a), Token::Num(b)) => a.cmp(&b),
    (Token::Text(a), Token::Text(b)) => a.cmp(b),
    (Token::Num(_), Token::Text(_)) => Ordering::Greater,
    (Token::Text(_), Token::Num(_)) => Ordering::Less,
  }
}

fn split_pkgver_rel(input: &str) -> (String, String) {
  if let Some((ver, rel)) = input.rsplit_once('-') {
    (ver.to_string(), rel.to_string())
  } else {
    (input.to_string(), String::new())
  }
}

/// Compares pkgrel segments with the same tokenizer pkgver uses.
///
/// A rel is not necessarily a bare number: opkg writes `1.2.3-1`, Alpine writes
/// `1.2.3-r1`, and either can reach pica from a manifest bound or from a feed
/// listing. So the rel is tokenized and compared pairwise — digits by value,
/// letters by name, a digit token beating a letter token at the same position.
/// When one side runs out of tokens first, the longer one is newer, because any
/// rel outranks no rel at all.
///
/// A missing rel, `-0` and `-r0` are the same version: opkg and apk both default
/// the revision to zero, so `1.2.3`, `1.2.3-0` and `1.2.3-r0` must stay equal —
/// otherwise `<=1.2.3` would refuse an installed `1.2.3-r0`.
fn compare_rel(a: &str, b: &str) -> Ordering {
  let a = if is_revision_zero(a) { "" } else { a };
  let b = if is_revision_zero(b) { "" } else { b };

  let a_tokens = tokenize_version(a);
  let b_tokens = tokenize_version(b);

  let len = a_tokens.len().max(b_tokens.len());
  for index in 0..len {
    match (a_tokens.get(index), b_tokens.get(index)) {
      (Some(&left), Some(&right)) => {
        let ord = compare_token(left, right);
        if ord != Ordering::Equal {
          return ord;
        }
      }
      (Some(_), None) => return Ordering::Greater,
      (None, Some(_)) => return Ordering::Less,
      (None, None) => break,
    }
  }
  Ordering::Equal
}

/// The three spellings of "no revision": absent, opkg's `-0`, apk's `-r0`.
fn is_revision_zero(rel: &str) -> bool {
  rel.is_empty() || rel == "0" || rel == "r0"
}

fn is_dot_numeric(input: &str) -> bool {
  if input.is_empty() {
    return false;
  }
  input.split('.').all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()))
}

fn compare_dot_numeric(a: &str, b: &str) -> Option<Ordering> {
  if !is_dot_numeric(a) || !is_dot_numeric(b) {
    return None;
  }

  let a_parts: Vec<u64> = a.split('.').map(|part| part.parse::<u64>().unwrap_or(0)).collect();
  let b_parts: Vec<u64> = b.split('.').map(|part| part.parse::<u64>().unwrap_or(0)).collect();

  let len = a_parts.len().max(b_parts.len());
  for index in 0..len {
    let left = *a_parts.get(index).unwrap_or(&0);
    let right = *b_parts.get(index).unwrap_or(&0);
    match left.cmp(&right) {
      Ordering::Equal => {}
      non_eq => return Some(non_eq),
    }
  }

  Some(Ordering::Equal)
}

#[cfg(test)]
mod tests {
  use super::{pkgver_cmp_key, pkgver_ge, pkgver_gt, ver_ge};

  #[test]
  fn compare_dot_versions() {
    assert!(ver_ge("0.1.5", "0.1.0"));
    assert!(ver_ge("1.0", "1.0.0"));
    assert!(!ver_ge("1.0.0", "1.0.1"));
  }

  #[test]
  fn compare_pkgver_rel() {
    assert!(pkgver_ge("1.2.3-2", "1.2.3-1"));
    assert!(pkgver_ge("1.2.4-1", "1.2.3-9"));
    assert!(!pkgver_ge("1.2.3-1", "1.2.3-2"));
    // `-0` is the default revision, so writing it out changes nothing.
    assert!(pkgver_ge("1.2.3", "1.2.3-0"));
    assert!(pkgver_ge("1.2.3-0", "1.2.3"));
  }

  #[test]
  fn compare_mixed_pkgver_rel() {
    assert!(!pkgver_ge("9.0-1", "10.0-1"));
    // `rc10` is strictly newer than `rc2`, not just >=: the rel segments are
    // compared with tokenize-then-numeric, not lexicographically.
    assert!(pkgver_gt("1.0.0-rc10", "1.0.0-rc2"));
    // `rsplit_once('-')` splits `1.0.0-rc1` into pkgver `1.0.0` + rel `rc1`.
    // Having a rel is newer than having none, so `1.0.0-rc1 > 1.0.0`. This
    // differs from schemes that treat `-rc` as a pre-release tag inside the
    // version; pica's split is positional, not semantic.
    assert!(pkgver_gt("1.0.0-rc1", "1.0.0"));
  }

  #[test]
  fn alpine_r_prefix_rel_is_ordered_correctly() {
    // `-rN` is Alpine's pkgrel syntax. Having a rel is newer than not having
    // one, and `r2` is newer than `r1`.
    assert!(pkgver_gt("1.2-r1", "1.2"));
    assert!(pkgver_gt("1.2-r2", "1.2-r1"));
    assert!(pkgver_ge("1.2-r2", "1.2-r1"));
    // `-r0` is the default revision, so it is the same version as no rel at all.
    assert!(pkgver_ge("1.2", "1.2-r0"));
    assert!(pkgver_ge("1.2-r0", "1.2"));
    assert!(pkgver_ge("1.2-r0", "1.2-0"));
  }

  #[test]
  fn strict_greater_rejects_equal_and_unset_bounds() {
    assert!(pkgver_gt("1.2.3-2", "1.2.3-1"));
    assert!(pkgver_gt("1.2.3", "1.2.2"));
    assert!(!pkgver_gt("1.2.3-1", "1.2.3-1"));
    assert!(!pkgver_gt("1.2.2", "1.2.3"));
    assert!(!pkgver_gt("1.2.3", ""));
    assert!(!pkgver_gt("", "1.2.3"));
  }

  #[test]
  fn cmp_key_compose() {
    assert_eq!(pkgver_cmp_key("1.2.3", "1"), "1.2.3-1");
    assert_eq!(pkgver_cmp_key("1.2.3", ""), "1.2.3");
  }
}
