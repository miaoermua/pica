//! Finding packages by part of a name or a word of the description.
//!
//! Matching is for searching and for suggesting a name after a miss. It never
//! picks what gets installed: `pica -S tail` must not install `tailscale`.

use std::cmp::Ordering;

/// How well a package matched, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rank {
  /// The name is the query, case aside.
  Exact,
  /// The name starts with the query.
  Prefix,
  /// The name contains the query.
  Name,
  /// The description contains the query.
  Description,
  /// The name is within a few edits of the query: a typo.
  Close,
}

/// What a candidate offers to match against.
#[derive(Debug, Clone, Copy)]
pub struct Fields<'a> {
  pub appname: &'a str,
  pub pkgname: &'a str,
  /// Every description the package carries.
  pub descriptions: &'a [&'a str],
}

/// The search terms, folded once.
#[derive(Debug, Clone)]
pub struct Query {
  terms: Vec<String>,
}

impl Query {
  /// Splits `text` into terms. Full-width brackets and colons become their
  /// ASCII forms, like a selector, and case is folded. Selector syntax is not
  /// interpreted: `foo@bar` is searched for as written.
  #[must_use]
  pub fn new(text: &str) -> Self {
    let folded = fold(text);
    Self { terms: folded.split_whitespace().map(ToString::to_string).collect() }
  }

  #[must_use]
  pub fn is_empty(&self) -> bool {
    self.terms.is_empty()
  }

  /// How `fields` matches, or `None`. Every term has to match (pacman's AND),
  /// and the weakest term decides the rank.
  #[must_use]
  pub fn rank(&self, fields: &Fields<'_>) -> Option<Rank> {
    if self.terms.is_empty() {
      return None;
    }
    self
      .terms
      .iter()
      .map(|term| rank_term(term, fields))
      .try_fold(Rank::Exact, |worst, rank| rank.map(|rank| worst.max(rank)))
  }
}

fn rank_term(term: &str, fields: &Fields<'_>) -> Option<Rank> {
  let names = [fold(fields.appname), fold(fields.pkgname)];
  let best = |rank: Rank, test: &dyn Fn(&str) -> bool| {
    names.iter().any(|name| !name.is_empty() && test(name)).then_some(rank)
  };
  best(Rank::Exact, &|name| name == term)
    .or_else(|| best(Rank::Prefix, &|name| name.starts_with(term)))
    .or_else(|| best(Rank::Name, &|name| name.contains(term)))
    .or_else(|| {
      fields.descriptions.iter().any(|text| fold(text).contains(term)).then_some(Rank::Description)
    })
    .or_else(|| best(Rank::Close, &|name| is_close(term, name)))
}

/// Names close enough to `query` to be what was meant, best first, at most
/// `limit`. Only for the hint after a miss.
#[must_use]
pub fn suggestions<'a>(
  query: &str,
  names: impl IntoIterator<Item = &'a str>,
  limit: usize,
) -> Vec<String> {
  let query = fold(query);
  let mut scored: Vec<(Rank, usize, String)> = Vec::new();
  for name in names {
    let folded = fold(name);
    if folded == query || folded.is_empty() {
      continue;
    }
    let rank = if folded.starts_with(&query) || query.starts_with(&folded) && query.len() > 3 {
      Rank::Prefix
    } else if query.len() >= 3 && folded.contains(&query) {
      Rank::Name
    } else if is_close(&query, &folded) {
      Rank::Close
    } else {
      continue;
    };
    let distance = edit_distance(&query, &folded);
    if !scored.iter().any(|(_, _, existing)| existing == name) {
      scored.push((rank, distance, name.to_string()));
    }
  }
  scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then_with(|| a.2.cmp(&b.2)));
  scored.into_iter().take(limit).map(|(_, _, name)| name).collect()
}

/// Whether `name` is a plausible typo of `term`. The allowance grows with the
/// length of what was typed: a three-letter name would match almost anything at
/// distance two.
fn is_close(term: &str, name: &str) -> bool {
  let allowed = match term.chars().count() {
    0..=3 => 0,
    4..=6 => 1,
    _ => 2,
  };
  allowed > 0 && term.is_ascii() && name.is_ascii() && edit_distance(term, name) <= allowed
}

fn fold(text: &str) -> String {
  text.trim().replace('：', ":").replace('（', "(").replace('）', ")").to_lowercase()
}

/// Levenshtein distance over bytes; both sides are ASCII when it matters.
fn edit_distance(a: &str, b: &str) -> usize {
  let (a, b) = (a.as_bytes(), b.as_bytes());
  let mut row: Vec<usize> = (0..=b.len()).collect();
  for (i, &ca) in a.iter().enumerate() {
    let mut diagonal = row[0];
    row[0] = i + 1;
    for (j, &cb) in b.iter().enumerate() {
      let above = row[j + 1];
      row[j + 1] = match ca.cmp(&cb) {
        Ordering::Equal => diagonal,
        _ => 1 + diagonal.min(above).min(row[j]),
      };
      diagonal = above;
    }
  }
  row[b.len()]
}

#[cfg(test)]
mod tests {
  use super::{Fields, Query, Rank, edit_distance, suggestions};
  use pretty_assertions::assert_eq;

  fn fields<'a>(appname: &'a str, descriptions: &'a [&'a str]) -> Fields<'a> {
    Fields { appname, pkgname: appname, descriptions }
  }

  #[test]
  fn ranks_go_from_exact_to_a_typo() {
    let rank = |query: &str, name: &str, desc: &str| Query::new(query).rank(&fields(name, &[desc]));
    assert_eq!(rank("Tailscale", "tailscale", ""), Some(Rank::Exact));
    assert_eq!(rank("tail", "tailscale", ""), Some(Rank::Prefix));
    assert_eq!(rank("scale", "tailscale", ""), Some(Rank::Name));
    assert_eq!(
      rank("wireguard", "tailscale", "Zero config VPN on WireGuard"),
      Some(Rank::Description)
    );
    assert_eq!(rank("tailscle", "tailscale", ""), Some(Rank::Close));
    assert_eq!(rank("frp", "nps", ""), None, "short names do not match by typo");
  }

  #[test]
  fn every_term_has_to_match_and_the_weakest_decides() {
    let query = Query::new("tail vpn");
    assert_eq!(query.rank(&fields("tailscale", &["a VPN"])), Some(Rank::Description));
    assert_eq!(query.rank(&fields("tailscale", &["a mesh"])), None);
  }

  #[test]
  fn chinese_descriptions_match_by_substring() {
    assert_eq!(
      Query::new("穿透").rank(&fields("frpc", &["内网穿透客户端"])),
      Some(Rank::Description)
    );
  }

  #[test]
  fn selector_syntax_is_searched_as_written() {
    assert_eq!(Query::new("demo@alice").rank(&fields("demo", &[])), None);
    let pkg = Fields { appname: "demo", pkgname: "demo@alice", descriptions: &[] };
    assert_eq!(Query::new("demo@alice").rank(&pkg), Some(Rank::Exact));
  }

  #[test]
  fn an_empty_query_matches_nothing() {
    assert_eq!(Query::new("  ").rank(&fields("demo", &[])), None);
  }

  #[test]
  fn suggestions_prefer_close_and_cap_the_list() {
    let names = ["tailscale", "tailscale-bin", "taildrop", "frpc", "nps"];
    assert_eq!(suggestions("tailscle", names, 5), ["tailscale"]);
    assert_eq!(suggestions("tail", names, 2), ["taildrop", "tailscale"]);
    assert_eq!(suggestions("zzz", names, 5), Vec::<String>::new());
  }

  #[test]
  fn edit_distance_counts_single_edits() {
    assert_eq!(edit_distance("kitten", "sitting"), 3);
    assert_eq!(edit_distance("", "abc"), 3);
    assert_eq!(edit_distance("same", "same"), 0);
  }
}
