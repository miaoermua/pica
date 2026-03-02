use std::cmp::Ordering;

#[must_use]
pub fn ver_ge(a: &str, b: &str) -> bool {
  compare_dot_numeric(a, b).is_some_and(|ord| ord != Ordering::Less)
}

#[must_use]
pub fn pkgver_cmp_key(pkgver: &str, pkgrel: &str) -> String {
  if pkgrel.is_empty() {
    pkgver.to_string()
  } else {
    format!("{pkgver}-{pkgrel}")
  }
}

#[must_use]
pub fn pkgver_ge(a: &str, b: &str) -> bool {
  if b.is_empty() {
    return true;
  }
  if a.is_empty() {
    return false;
  }

  let (a_ver, a_rel) = split_pkgver_rel(a);
  let (b_ver, b_rel) = split_pkgver_rel(b);

  let is_numeric_a = is_dot_numeric(&a_ver);
  let is_numeric_b = is_dot_numeric(&b_ver);

  if is_numeric_a && is_numeric_b {
    match compare_dot_numeric(&a_ver, &b_ver) {
      Some(Ordering::Greater) => true,
      Some(Ordering::Equal) => compare_numeric_rel(&a_rel, &b_rel) != Ordering::Less,
      Some(Ordering::Less) | None => false,
    }
  } else {
    match compare_mixed_version(&a_ver, &b_ver) {
      Ordering::Greater => true,
      Ordering::Equal => compare_numeric_rel(&a_rel, &b_rel) != Ordering::Less,
      Ordering::Less => false,
    }
  }
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
  let mut start = 0usize;
  let chars: Vec<char> = input.chars().collect();

  while start < chars.len() {
    if !chars[start].is_ascii_alphanumeric() {
      start += 1;
      continue;
    }

    let is_digit = chars[start].is_ascii_digit();
    let mut end = start + 1;
    while end < chars.len() {
      let ch = chars[end];
      if !ch.is_ascii_alphanumeric() || ch.is_ascii_digit() != is_digit {
        break;
      }
      end += 1;
    }

    let segment = &input[chars[..start].iter().map(|ch| ch.len_utf8()).sum::<usize>()
      ..chars[..end].iter().map(|ch| ch.len_utf8()).sum::<usize>()];

    if is_digit {
      let value = segment.parse::<u64>().unwrap_or(0);
      out.push(Token::Num(value));
    } else {
      out.push(Token::Text(segment));
    }

    start = end;
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
    (input.to_string(), "0".to_string())
  }
}

fn compare_numeric_rel(a: &str, b: &str) -> Ordering {
  match (a.parse::<u64>(), b.parse::<u64>()) {
    (Ok(a_num), Ok(b_num)) => a_num.cmp(&b_num),
    _ => Ordering::Equal,
  }
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
  use super::{pkgver_cmp_key, pkgver_ge, ver_ge};

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
    assert!(pkgver_ge("1.2.3", "1.2.3-0"));
  }

  #[test]
  fn compare_mixed_pkgver_rel() {
    assert!(!pkgver_ge("9.0-1", "10.0-1"));
    assert!(pkgver_ge("1.0.0-rc10", "1.0.0-rc2"));
    assert!(pkgver_ge("1.0.0", "1.0.0-rc1"));
  }

  #[test]
  fn cmp_key_compose() {
    assert_eq!(pkgver_cmp_key("1.2.3", "1"), "1.2.3-1");
    assert_eq!(pkgver_cmp_key("1.2.3", ""), "1.2.3");
  }
}
