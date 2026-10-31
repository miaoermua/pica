//! Version bounds and optionality in a manifest's `app` / `base` / `kmod` lists.
//!
//! A declaration is either a bare name (`libfoo`) or a name with one bound
//! (`libfoo>=1.2`). The bound is an assertion pica checks against what the
//! system has installed — never solver input. It is deliberately not handed to
//! a backend: `apk` accepts a versioned atom, `opkg` installs by name or by
//! `.ipk` path, and one string cannot mean the same thing to both.
//!
//! A trailing `?` marks the declaration as optional (`libfoo>=1.2?`). Optional
//! means "offer it to the user", never "not needed": the name still names a
//! package the manifest wants, so the precheck reports it and the installer can
//! choose it. `?` is not a package-name character, so the marker cannot be
//! confused with a real name, and a manifest without it parses as before.

use crate::error::{PicaError, PicaResult};
use crate::version::pkgver_cmp;
use std::cmp::Ordering;
use std::collections::HashSet;

/// The comparison a bound asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
  Ge,
  Le,
  Gt,
  Lt,
  Eq,
}

impl Op {
  #[must_use]
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Ge => ">=",
      Self::Le => "<=",
      Self::Gt => ">",
      Self::Lt => "<",
      Self::Eq => "=",
    }
  }

  /// Whether `installed` satisfies this operator against `required`.
  ///
  /// The comparison is [`pkgver_cmp`], so a `pkgver-pkgrel` pair is ordered end
  /// to end: `1.2-1` is newer than `1.2`, which is how opkg and apk read them.
  #[must_use]
  pub fn is_satisfied_by(self, installed: &str, required: &str) -> bool {
    let order = pkgver_cmp(installed, required);
    match self {
      Self::Ge => order != Ordering::Less,
      Self::Le => order != Ordering::Greater,
      Self::Gt => order == Ordering::Greater,
      Self::Lt => order == Ordering::Less,
      Self::Eq => order == Ordering::Equal,
    }
  }
}

/// One declaration from `app` / `base` / `kmod`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Constraint {
  /// The declaration as written, trimmed. Reported verbatim by `-Qi` and in
  /// messages, so `libfoo ≥ 1.2` is not silently rewritten to ASCII there.
  pub raw: String,
  /// The package the backend is asked about.
  pub name: String,
  /// The bound, when the declaration carries one.
  pub bound: Option<(Op, String)>,
  /// Whether the declaration ended with `?`.
  ///
  /// Optional declarations are still declarations: they are reported by the
  /// precheck and can be selected for installation. What they lose is the right
  /// to stop a install when they are missing.
  pub optional: bool,
}

impl Constraint {
  /// Parses one declaration.
  ///
  /// # Errors
  /// Returns an error for an empty declaration, a missing name or version, and
  /// for anything that spells two operators or two bounds in one string.
  pub fn parse(raw: &str) -> PicaResult<Self> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
      return Err(PicaError::msg("empty dependency declaration"));
    }

    // The marker is stripped before anything else so `>=` after a name is never
    // read as part of an optional marker. `?` cannot appear in a package name,
    // so a trailing one is unambiguous; `??` leaves `?` in the name and is
    // refused by the name check rather than being read as "very optional".
    let normalized = normalize_operators(trimmed);
    let (body, optional) = split_optional(&normalized);

    let Some(index) = body.find(['<', '>', '=']) else {
      let name = body.trim();
      require_name(name, trimmed)?;
      return Ok(Self { raw: trimmed.to_string(), name: name.to_string(), bound: None, optional });
    };

    let rest = &body[index..];
    let (op, version) = if let Some(tail) = rest.strip_prefix(">=") {
      (Op::Ge, tail)
    } else if let Some(tail) = rest.strip_prefix("<=") {
      (Op::Le, tail)
    } else if let Some(tail) = rest.strip_prefix('>') {
      (Op::Gt, tail)
    } else if let Some(tail) = rest.strip_prefix('<') {
      (Op::Lt, tail)
    } else if let Some(tail) = rest.strip_prefix('=') {
      (Op::Eq, tail)
    } else {
      return Err(PicaError::msg(format!("invalid operator in dependency: {trimmed}")));
    };

    let version = version.trim();
    if version.is_empty() {
      return Err(PicaError::msg(format!("missing version in dependency: {trimmed}")));
    }
    // A second operator, or a space-separated tail, is one of the shapes that
    // must not be guessed at: it could be a range the user meant, and picking
    // either end would silently drop half of it.
    if version.contains(['<', '>', '=']) || version.chars().any(char::is_whitespace) {
      return Err(PicaError::msg(format!("invalid operator or version in dependency: {trimmed}")));
    }

    let name = body[..index].trim();
    require_name(name, trimmed)?;

    Ok(Self {
      raw: trimmed.to_string(),
      name: name.to_string(),
      bound: Some((op, version.to_string())),
      optional,
    })
  }

  /// Whether the declaration carries a bound at all.
  #[must_use]
  pub fn has_bound(&self) -> bool {
    self.bound.is_some()
  }

  /// The bound as `op` + `version`, or an empty string when there is none.
  #[must_use]
  pub fn requirement(&self) -> String {
    self.bound.as_ref().map(|(op, version)| format!("{}{version}", op.as_str())).unwrap_or_default()
  }

  /// Whether an installed `version` meets this declaration.
  ///
  /// A declaration with no bound is met by any version, which matches what a
  /// bare name has always meant.
  #[must_use]
  pub fn is_met_by(&self, version: &str) -> bool {
    match &self.bound {
      None => true,
      Some((op, required)) => op.is_satisfied_by(version, required),
    }
  }
}

/// Parses every declaration in `items`, skipping the empty ones.
///
/// An empty item is not a declaration with a nameless package: `get_array_for`
/// returns `[""]` for a key that exists but is empty, and that is exactly how a
/// backend override says "this backend has no dependencies" (`base@apk =`).
/// Skipping it keeps that meaning instead of rejecting the manifest.
///
/// # Errors
/// Returns the first invalid declaration, as [`Constraint::parse`] does.
pub fn parse_all(items: &[String]) -> PicaResult<Vec<Constraint>> {
  items.iter().filter(|item| !item.trim().is_empty()).map(|item| Constraint::parse(item)).collect()
}

/// Parses a dependency list, dropping repeats and letting the required form win.
///
/// A name listed twice is one dependency, not two installs. When the two forms
/// disagree about optionality, the required one is kept: the manifest said the
/// package must be there once, and a second, weaker mention cannot take that
/// back. The surviving declaration keeps its own bound and its own position, so
/// a `libfoo>=1.2?` before a `libfoo` is reported as the bounded required form.
///
/// # Errors
/// Returns the first invalid declaration, as [`Constraint::parse`] does.
pub fn resolve(items: &[String]) -> PicaResult<Vec<Constraint>> {
  let parsed = parse_all(items)?;
  let required: HashSet<String> = parsed
    .iter()
    .filter(|constraint| !constraint.optional)
    .map(|constraint| constraint.name.clone())
    .collect();

  let mut seen = HashSet::new();
  let mut resolved = Vec::with_capacity(parsed.len());
  for constraint in parsed {
    if constraint.optional && required.contains(constraint.name.as_str()) {
      continue;
    }
    if !seen.insert(constraint.name.clone()) {
      continue;
    }
    resolved.push(constraint);
  }
  Ok(resolved)
}

/// The declarations that must be installed for the package to work.
pub fn required(constraints: &[Constraint]) -> impl Iterator<Item = &Constraint> {
  constraints.iter().filter(|constraint| !constraint.optional)
}

/// The declarations the user may decline.
pub fn optional(constraints: &[Constraint]) -> impl Iterator<Item = &Constraint> {
  constraints.iter().filter(|constraint| constraint.optional)
}

/// Splits a trailing `?` off `input`, returning the declaration without it.
fn split_optional(input: &str) -> (&str, bool) {
  match input.strip_suffix('?') {
    Some(body) => (body.trim_end(), true),
    None => (input, false),
  }
}

/// The names behind `constraints`, which is the only part a backend may see.
///
/// Keeping this a function rather than a field means a caller cannot
/// accidentally pass the declaration text on: the bound has no backend-neutral
/// spelling, so it is dropped here once instead of at each call site.
#[must_use]
pub fn names(constraints: &[Constraint]) -> Vec<String> {
  constraints.iter().map(|constraint| constraint.name.clone()).collect()
}

/// Full-width operators as an IME or a copied table produces them, folded to
/// the ASCII the parser reads. `selector` normalizes `：`/`＠` for the same
/// reason.
fn normalize_operators(input: &str) -> String {
  let mut out = String::with_capacity(input.len());
  for ch in input.chars() {
    match ch {
      '≥' => out.push_str(">="),
      '≤' => out.push_str("<="),
      '＞' => out.push('>'),
      '＜' => out.push('<'),
      '＝' => out.push('='),
      '？' => out.push('?'),
      other => out.push(other),
    }
  }
  out
}

/// Whether `name` is spellable as an opkg/apk package name.
///
/// The name is what a backend is later asked to install, so it has to look like
/// one: `[A-Za-z0-9._+-]`. Without this check `libfoo!=1.2` would parse as a
/// package called `libfoo!` and only fail at the precheck, as "missing
/// dependency libfoo!" — a message pointing at the wrong problem.
fn is_valid_name(name: &str) -> bool {
  name.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '+' | '-'))
}

fn require_name(name: &str, raw: &str) -> PicaResult<()> {
  if name.is_empty() {
    return Err(PicaError::msg(format!("missing package name in dependency: {raw}")));
  }
  if !is_valid_name(name) {
    return Err(PicaError::msg(format!("invalid package name in dependency: {raw}")));
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::{Constraint, Op, names, parse_all, resolve};

  fn parsed(raw: &str) -> Constraint {
    Constraint::parse(raw).expect("constraint parses")
  }

  #[test]
  fn a_bare_name_has_no_bound() {
    let constraint = parsed("libfoo");
    assert_eq!(constraint.name, "libfoo");
    assert_eq!(constraint.raw, "libfoo");
    assert_eq!(constraint.bound, None);
    assert_eq!(constraint.requirement(), "");
    assert!(constraint.is_met_by("0.0.1"));
  }

  #[test]
  fn every_operator_parses() {
    for (raw, op, version) in [
      ("libfoo>=1.2", Op::Ge, "1.2"),
      ("libfoo<=1.2", Op::Le, "1.2"),
      ("libfoo>1.2", Op::Gt, "1.2"),
      ("libfoo<1.2", Op::Lt, "1.2"),
      ("libfoo=1.2", Op::Eq, "1.2"),
    ] {
      let constraint = parsed(raw);
      assert_eq!(constraint.name, "libfoo", "{raw}");
      assert_eq!(constraint.bound, Some((op, version.to_string())), "{raw}");
      assert_eq!(constraint.requirement(), format!("{}{version}", op.as_str()), "{raw}");
    }
  }

  #[test]
  fn spaces_around_the_operator_are_not_part_of_the_name() {
    let constraint = parsed("  libfoo >= 1.2  ");
    assert_eq!(constraint.name, "libfoo");
    assert_eq!(constraint.bound, Some((Op::Ge, "1.2".to_string())));
    assert_eq!(constraint.raw, "libfoo >= 1.2");
  }

  #[test]
  fn full_width_operators_are_folded() {
    for (raw, op) in [
      ("libfoo≥1.2", Op::Ge),
      ("libfoo≤1.2", Op::Le),
      ("libfoo＞1.2", Op::Gt),
      ("libfoo＜1.2", Op::Lt),
      ("libfoo＝1.2", Op::Eq),
    ] {
      let constraint = parsed(raw);
      assert_eq!(constraint.name, "libfoo", "{raw}");
      assert_eq!(constraint.bound, Some((op, "1.2".to_string())), "{raw}");
      assert_eq!(constraint.raw, raw, "the original text is kept for display");
    }
  }

  #[test]
  fn the_whitelist_keeps_real_package_names() {
    // `.` `_` `+` `-` are all part of opkg/apk names; a whitelist that dropped
    // them would refuse packages that exist (`libstdc++`, `python3.11`).
    for raw in ["libstdc++", "python3.11>=1.0", "ca-certificates_2023", "kmod-nf_tables"] {
      assert!(Constraint::parse(raw).is_ok(), "{raw} must parse");
    }
  }

  #[test]
  fn invalid_declarations_are_refused() {
    for raw in [
      "",
      "   ",
      ">=1.2",
      "libfoo>=",
      "libfoo>",
      "libfoo>>1.2",
      "libfoo==1.2",
      "libfoo>1.2<2.0",
      "libfoo>=1.2 beta",
      "lib foo>=1.2",
      // `!` and `$` are not package-name characters. Left to the precheck,
      // `libfoo!=1.2` would be reported as a missing package named `libfoo!`.
      "libfoo!=1.2",
      "libfoo$>=1.2",
    ] {
      assert!(Constraint::parse(raw).is_err(), "{raw:?} must be refused");
    }
  }

  #[test]
  fn empty_items_mean_no_dependencies() {
    // `base@apk =` reads back as `[""]`, and that must stay "nothing declared".
    assert_eq!(parse_all(&[String::new()]).expect("empty list parses"), Vec::new());
    assert_eq!(parse_all(&[" ".to_string()]).expect("blank list parses"), Vec::new());

    let list = parse_all(&["libfoo>=1.0".to_string(), String::new()]).expect("parses");
    assert_eq!(names(&list), vec!["libfoo".to_string()]);
  }

  #[test]
  fn each_operator_judges_a_version() {
    assert!(parsed("libfoo>=1.2").is_met_by("1.2"));
    assert!(parsed("libfoo>=1.2").is_met_by("1.3"));
    assert!(!parsed("libfoo>=1.2").is_met_by("1.1"));

    assert!(parsed("libfoo<=1.2").is_met_by("1.2"));
    assert!(parsed("libfoo<=1.2").is_met_by("1.1"));
    assert!(!parsed("libfoo<=1.2").is_met_by("1.3"));

    assert!(parsed("libfoo>1.2").is_met_by("1.3"));
    assert!(!parsed("libfoo>1.2").is_met_by("1.2"));

    assert!(parsed("libfoo<1.2").is_met_by("1.1"));
    assert!(!parsed("libfoo<1.2").is_met_by("1.2"));

    assert!(parsed("libfoo=1.2").is_met_by("1.2"));
    assert!(!parsed("libfoo=1.2").is_met_by("1.2.1"));
  }

  #[test]
  fn the_pkgrel_participates_in_the_comparison() {
    // opkg and apk both order `1.2-1` above `1.2`: the release is part of the
    // version, so a bound written without one excludes later releases only as
    // far as the pkgver itself goes.
    assert!(parsed("libfoo>=1.2").is_met_by("1.2-1"));
    assert!(!parsed("libfoo<=1.2").is_met_by("1.2-1"));
    assert!(parsed("libfoo>1.2-1").is_met_by("1.2-2"));
    assert!(!parsed("libfoo>1.2-1").is_met_by("1.2-1"));

    // Alpine's `-rN` is a rel like any other: `>` must not call an installed
    // `-r1` too old for `>1.2`, and `>=1.2-r2` must not accept `1.2-r1`.
    assert!(parsed("libfoo>1.2").is_met_by("1.2-r1"));
    assert!(!parsed("libfoo>=1.2-r2").is_met_by("1.2-r1"));
  }

  #[test]
  fn names_drops_the_bounds() {
    let list = parse_all(&["libfoo>=1.2".to_string(), "libbar".to_string()]).expect("parses");
    assert_eq!(names(&list), vec!["libfoo".to_string(), "libbar".to_string()]);
  }

  #[test]
  fn a_trailing_question_mark_marks_a_declaration_optional() {
    let constraint = parsed("libfoo?");
    assert_eq!(constraint.name, "libfoo");
    assert!(constraint.optional, "the marker is not part of the name");
    assert_eq!(constraint.raw, "libfoo?", "the marker is kept for display");
    assert!(!constraint.has_bound());
    assert!(constraint.is_met_by("0.0.1"), "optional still means the packages the manifest wants");

    let bounded = parsed("libfoo>=1.2?");
    assert_eq!(bounded.name, "libfoo");
    assert_eq!(bounded.bound, Some((Op::Ge, "1.2".to_string())));
    assert!(bounded.optional);

    // Whitespace before the marker is not part of the name, and the full-width
    // question mark an IME produces is the same marker.
    assert_eq!(parsed("libfoo >= 1.2 ?").name, "libfoo");
    assert!(parsed("libfoo≥1.2？").optional);

    // A manifest written before the marker existed keeps its meaning.
    assert!(!parsed("libfoo").optional);
    assert!(!parsed("libfoo>=1.2").optional);
  }

  #[test]
  fn a_misplaced_optional_marker_is_refused() {
    for raw in ["?", "  ?  ", "??", "libfoo??", "libfoo?>=1.2", "libfoo? bar"] {
      assert!(Constraint::parse(raw).is_err(), "{raw:?} must be refused");
    }
  }

  #[test]
  fn resolve_drops_repeats_and_keeps_the_required_form() {
    let resolved = resolve(&[
      "libfoo>=1.0?".to_string(),
      "libbar".to_string(),
      "libfoo".to_string(),
      "libbar?".to_string(),
      "libbaz>=2?".to_string(),
      "libbaz?".to_string(),
      String::new(),
    ])
    .expect("resolves");

    let summary: Vec<(String, bool, String)> =
      resolved.iter().map(|c| (c.name.clone(), c.optional, c.requirement())).collect();
    assert_eq!(
      summary,
      vec![
        // `libbar` and `libfoo` were required somewhere, so the optional
        // mentions disappear; `libfoo` keeps the required declaration's
        // (unbounded) form and its own position.
        ("libbar".to_string(), false, String::new()),
        ("libfoo".to_string(), false, String::new()),
        // Repeated optionals collapse to the first mention, bound included.
        ("libbaz".to_string(), true, ">=2".to_string()),
      ]
    );
  }
}
