use std::cmp::Ordering;

pub fn ver_ge(a: &str, b: &str) -> bool {
    compare_dot_numeric(a, b).is_some_and(|ord| ord != Ordering::Less)
}

pub fn pkgver_cmp_key(pkgver: &str, pkgrel: &str) -> String {
    if pkgrel.is_empty() {
        pkgver.to_string()
    } else {
        format!("{pkgver}-{pkgrel}")
    }
}

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
        a >= b
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
    input
        .split('.')
        .all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()))
}

fn compare_dot_numeric(a: &str, b: &str) -> Option<Ordering> {
    if !is_dot_numeric(a) || !is_dot_numeric(b) {
        return None;
    }

    let a_parts: Vec<u64> = a
        .split('.')
        .map(|part| part.parse::<u64>().unwrap_or(0))
        .collect();
    let b_parts: Vec<u64> = b
        .split('.')
        .map(|part| part.parse::<u64>().unwrap_or(0))
        .collect();

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
    fn cmp_key_compose() {
        assert_eq!(pkgver_cmp_key("1.2.3", "1"), "1.2.3-1");
        assert_eq!(pkgver_cmp_key("1.2.3", ""), "1.2.3");
    }
}
