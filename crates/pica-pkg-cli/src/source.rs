//! Where a source may live, and how a relative path inside one is turned into
//! a URL.
//!
//! A source root is whatever the user configured: a GitHub raw directory, a
//! Worker origin, an `http://192.168.1.10/repo` on the LAN, or a `file://`
//! path. Everything a source itself hands us — a manager index path, a
//! `download_url` — is resolved against that root and then checked here, so a
//! source cannot point the client at an arbitrary scheme or climb out of the
//! directory it was trusted for.
//!
//! Plain HTTP is not refused outright: a LAN mirror is a normal way to run a
//! source, and demanding TLS on `192.168.1.10` would only push people to
//! `--allow-insecure`-style workarounds. It is refused for anything that is
//! not this machine or the local network, because there the transport can be
//! rewritten by anyone on the path.

use crate::app::{CliError, CliResult, E_CONFIG_INVALID, E_POLICY_INVALID};
use pica_pkg_core::repo::relative_path_problem;

/// How far a redirect chain may go before the client gives up.
pub(crate) const MAX_REDIRECTS: u32 = 5;

/// Which part of the network a URL points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostScope {
  /// This machine: `localhost`, `127.0.0.0/8`, `::1`.
  Loopback,
  /// A private network: `10/8`, `172.16/12`, `192.168/16`, `fc00::/7`.
  Private,
  /// `169.254/16`, `fe80::/10` — a link, still local.
  LinkLocal,
  /// Everything else, including every name we cannot resolve without asking
  /// the network.
  Public,
}

impl HostScope {
  fn is_local(self) -> bool {
    matches!(self, Self::Loopback | Self::Private | Self::LinkLocal)
  }
}

/// What this client is willing to fetch, and from where.
///
/// The policy travels with the fetch rather than being decided by the fetcher,
/// so the same rules apply to a source root, a manager index and a package.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FetchPolicy {
  allow_public_http: bool,
  allow_file: bool,
}

impl FetchPolicy {
  /// A policy for a source root: `file://` is fine (a local mirror), and the
  /// source's own `allow_insecure_http` decides about public plain HTTP.
  #[must_use]
  pub(crate) fn source(allow_public_http: bool) -> Self {
    Self { allow_public_http, allow_file: true }
  }

  /// A policy for something a *remote* source asked us to download. It may not
  /// reach into this machine's filesystem; a local file is only a source, not a
  /// download target of a source that is not itself local.
  #[must_use]
  pub(crate) fn download(allow_public_http: bool, allow_file: bool) -> Self {
    Self { allow_public_http, allow_file }
  }

  /// A policy for a URL the user typed (`-U`, `-Sp`). The user is the trust
  /// anchor there, so nothing is refused beyond an unusable scheme.
  #[must_use]
  pub(crate) fn explicit() -> Self {
    Self { allow_public_http: true, allow_file: true }
  }

  /// # Errors
  /// Returns an error naming the scheme, host or scope that is refused.
  pub(crate) fn check(self, url: &str) -> CliResult<()> {
    let parts = split(url)
      .ok_or_else(|| CliError::new(E_POLICY_INVALID, format!("not an absolute URL: {url}")))?;

    if !parts.authority.is_empty() && parts.authority.contains('@') {
      return Err(CliError::new(
        E_POLICY_INVALID,
        format!("refusing a URL with credentials in it: {url}"),
      ));
    }

    match parts.scheme {
      "file" => {
        if self.allow_file {
          Ok(())
        } else {
          Err(CliError::new(
            E_POLICY_INVALID,
            format!("a remote source may not download from this machine: {url}"),
          ))
        }
      }
      "https" => Ok(()),
      "http" => {
        let scope = host_scope(parts.authority);
        if scope.is_local() || self.allow_public_http {
          Ok(())
        } else {
          Err(CliError::new(
            E_POLICY_INVALID,
            format!(
              "plain HTTP is only used for local sources; {url} is public — use https, or set \
               allow_insecure_http for this source if that is really what you want"
            ),
          ))
        }
      }
      other => {
        Err(CliError::new(E_POLICY_INVALID, format!("unsupported URL scheme {other}: {url}")))
      }
    }
  }

  /// # Errors
  /// Refuses a hop the policy would not have followed on its own, and refuses
  /// any downgrade from TLS: a redirect must not be able to undo the reason the
  /// first URL was acceptable.
  pub(crate) fn check_redirect(self, from: &str, to: &str) -> CliResult<()> {
    self.check(to)?;

    if scheme_of(from) == Some("https") && scheme_of(to) == Some("http") {
      return Err(CliError::new(
        E_POLICY_INVALID,
        format!("refusing to follow a redirect from https to {to}"),
      ));
    }

    Ok(())
  }
}

/// The policy that applies to a package downloaded on behalf of `root`.
///
/// A source that is itself local — a `file://` directory, and also an index
/// written before sources carried a root at all — may hand out `file://`
/// package URLs, because that is what a local mirror is. Only a source reached
/// over the network is refused that reach into this machine. Whether a public
/// plain-HTTP host is acceptable is the source's own `allow_insecure_http`.
#[must_use]
pub(crate) fn download_policy(root: &str, allow_public_http: bool) -> FetchPolicy {
  let remote = matches!(scheme_of(root), Some("http" | "https"));
  FetchPolicy::download(allow_public_http, !remote)
}

/// The pieces of an absolute URL this client cares about.
#[derive(Debug, Clone, Copy)]
pub(crate) struct UrlParts<'a> {
  pub(crate) scheme: &'a str,
  /// Empty for `file:///path`; includes the port when one was written.
  pub(crate) authority: &'a str,
  pub(crate) path: &'a str,
}

/// Whether `url` is exactly the case [`FetchPolicy::check`] refuses by default:
/// plain HTTP to somewhere that is not this machine or the local network.
///
/// The wizard asks about this one case and nothing else, so it needs the same
/// rule as the check rather than a second opinion on what "public" means.
#[must_use]
pub(crate) fn needs_insecure_http_opt_in(url: &str) -> bool {
  split(url).is_some_and(|parts| {
    parts.scheme == "http" && !parts.authority.is_empty() && !host_scope(parts.authority).is_local()
  })
}

/// Split `url` into scheme, authority and path. Returns `None` for anything
/// that is not `scheme://...` with a scheme this client could ever fetch.
#[must_use]
pub(crate) fn split(url: &str) -> Option<UrlParts<'_>> {
  let (scheme, rest) = url.split_once("://")?;
  if scheme.is_empty()
    || !scheme.chars().all(|ch| {
      ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '+' || ch == '-' || ch == '.'
    })
  {
    return None;
  }

  let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
  let (authority, tail) = rest.split_at(end);
  let path = tail.split(['?', '#']).next().unwrap_or("");

  Some(UrlParts { scheme, authority, path })
}

#[must_use]
pub(crate) fn scheme_of(url: &str) -> Option<&str> {
  split(url).map(|parts| parts.scheme)
}

/// Where `authority` sits. Names are public unless they say `localhost`: this
/// client does not resolve names just to classify them, and treating an
/// unresolved name as local would wave through exactly the public host that
/// plain HTTP must not be used with.
#[must_use]
pub(crate) fn host_scope(authority: &str) -> HostScope {
  let host = host_of(authority);

  if host.eq_ignore_ascii_case("localhost") || host.to_ascii_lowercase().ends_with(".localhost") {
    return HostScope::Loopback;
  }

  if let Some([a, b, ..]) = ipv4_octets(host) {
    return match (a, b) {
      (127, _) | (0, 0) => HostScope::Loopback,
      (10, _) | (192, 168) => HostScope::Private,
      (172, second) if (16..=31).contains(&second) => HostScope::Private,
      (169, 254) => HostScope::LinkLocal,
      _ => HostScope::Public,
    };
  }

  let v6 = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
  if v6 == "::1" {
    return HostScope::Loopback;
  }
  if v6.starts_with("fe80:") || v6.starts_with("fe80::") {
    return HostScope::LinkLocal;
  }
  if v6.starts_with("fc") || v6.starts_with("fd") {
    return HostScope::Private;
  }

  HostScope::Public
}

fn host_of(authority: &str) -> &str {
  let without_user = authority.rsplit('@').next().unwrap_or(authority);
  if let Some(rest) = without_user.strip_prefix('[') {
    return rest.split(']').next().map_or(without_user, |inside| {
      // Hand back the bracketed form so IPv6 detection sees one shape.
      &without_user[..inside.len() + 2]
    });
  }
  without_user.split(':').next().unwrap_or(without_user)
}

fn ipv4_octets(host: &str) -> Option<[u8; 4]> {
  let mut octets = [0_u8; 4];
  let mut parts = host.split('.');
  for octet in &mut octets {
    let part = parts.next()?;
    if part.is_empty() || part.len() > 3 || !part.chars().all(|ch| ch.is_ascii_digit()) {
      return None;
    }
    *octet = part.parse().ok()?;
  }
  if parts.next().is_some() { None } else { Some(octets) }
}

/// Turn a path a source published into an absolute URL under `root`.
///
/// `relative` must be a plain relative path: no scheme, no leading slash, no
/// `..`, no percent-encoding. The result is still checked by [`FetchPolicy`]
/// before anything is fetched.
///
/// # Errors
/// Returns an error when `root` is not an absolute URL or `relative` is not a
/// safe relative path.
pub(crate) fn join_child(root: &str, relative: &str) -> CliResult<String> {
  if let Some(reason) = relative_path_problem(relative) {
    return Err(CliError::new(
      E_CONFIG_INVALID,
      format!("{relative:?} is not a safe relative path: {reason}"),
    ));
  }

  let parts = split(root).ok_or_else(|| {
    CliError::new(E_CONFIG_INVALID, format!("source root is not an absolute URL: {root}"))
  })?;

  let root_path = parts.path.trim_end_matches('/');
  Ok(format!("{}://{}{root_path}/{relative}", parts.scheme, parts.authority))
}

/// Turn a path a document published into an absolute URL, relative to the
/// directory that document lives in.
///
/// This is the rule for anything a file hands us about its neighbours: a
/// descriptor's `managers[].path` is relative to the descriptor, not to itself.
///
/// # Errors
/// Returns an error when `file_url` is not an absolute URL or `relative` is not
/// a safe relative path.
pub(crate) fn join_sibling(file_url: &str, relative: &str) -> CliResult<String> {
  let parts = split(file_url)
    .ok_or_else(|| CliError::new(E_CONFIG_INVALID, format!("not an absolute URL: {file_url}")))?;

  let dir = match parts.path.rfind('/') {
    Some(index) => &parts.path[..=index],
    None => "/",
  };

  join_child(&format!("{}://{}{dir}", parts.scheme, parts.authority), relative)
}

/// Resolve the `Location` of a redirect against the URL that produced it.
///
/// Handles the three shapes servers actually send: absolute URLs, absolute
/// paths (`/next`), and plain relative paths. `..` segments are folded here
/// rather than trusted, and the walk never climbs above the host root.
///
/// # Errors
/// Returns an error for a `Location` this client cannot place (for example a
/// protocol-relative URL, which would change the scheme).
pub(crate) fn resolve_location(current: &str, location: &str) -> CliResult<String> {
  let location = location.trim();
  if location.is_empty() {
    return Err(CliError::new(E_POLICY_INVALID, "redirect without a Location value"));
  }

  if split(location).is_some() {
    return Ok(location.to_string());
  }

  if location.starts_with("//") {
    return Err(CliError::new(
      E_POLICY_INVALID,
      format!("refusing a protocol-relative redirect: {location}"),
    ));
  }

  let parts = split(current).ok_or_else(|| {
    CliError::new(E_POLICY_INVALID, format!("cannot resolve a redirect from {current}"))
  })?;

  let (path, query) = match location.split_once('?') {
    Some((path, query)) => (path, Some(query)),
    None => (location, None),
  };

  let base = if path.starts_with('/') {
    String::new()
  } else {
    let trimmed = parts.path.trim_end_matches('/');
    match trimmed.rfind('/') {
      Some(index) => trimmed[..=index].to_string(),
      None => "/".to_string(),
    }
  };

  let joined = format!("{base}{path}");
  let mut segments: Vec<&str> = Vec::new();
  for segment in joined.split('/') {
    match segment {
      "" | "." => {}
      ".." => {
        segments.pop();
      }
      other => segments.push(other),
    }
  }

  let path = format!("/{}", segments.join("/"));
  let query = query.map_or(String::new(), |value| format!("?{value}"));
  Ok(format!("{}://{}{path}{query}", parts.scheme, parts.authority))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_local_source_may_be_plain_http_or_a_directory() {
    assert!(FetchPolicy::source(false).check("http://127.0.0.1:8080/r/index.json").is_ok());
    assert!(FetchPolicy::source(false).check("http://192.168.1.10/repo/index.json").is_ok());
    assert!(FetchPolicy::source(false).check("http://localhost/r/index.json").is_ok());
    assert!(FetchPolicy::source(false).check("file:///srv/pica/index.json").is_ok());
  }

  #[test]
  fn a_public_source_may_not_be_plain_http_unless_it_says_so() {
    let err = FetchPolicy::source(false)
      .check("http://example.com/r/index.json")
      .expect_err("public plain http");
    assert_eq!(err.code, E_POLICY_INVALID);
    assert!(err.message.contains("allow_insecure_http"), "{}", err.message);

    assert!(FetchPolicy::source(true).check("http://example.com/r/index.json").is_ok());
    assert!(FetchPolicy::source(false).check("https://example.com/r/index.json").is_ok());
  }

  #[test]
  fn a_remote_source_may_not_reach_into_this_machine() {
    let err = FetchPolicy::download(false, false)
      .check("file:///etc/passwd")
      .expect_err("remote source, local file");
    assert!(err.message.contains("remote source"), "{}", err.message);
  }

  #[test]
  fn unknown_schemes_are_refused_with_their_name() {
    let err = FetchPolicy::explicit().check("ftp://example.com/x").expect_err("ftp");
    assert!(err.message.contains("ftp"), "{}", err.message);
    assert!(FetchPolicy::explicit().check("not a url").is_err());
  }

  #[test]
  fn credentials_in_a_url_are_refused() {
    let err = FetchPolicy::explicit()
      .check("https://user:pass@example.com/r/index.json")
      .expect_err("userinfo");
    assert!(err.message.contains("credentials"), "{}", err.message);
  }

  #[test]
  fn a_redirect_may_not_undo_tls() {
    let policy = FetchPolicy::source(true);
    let err = policy
      .check_redirect("https://example.com/a", "http://192.168.1.10/b")
      .expect_err("https to http");
    assert!(err.message.contains("https"), "{}", err.message);
    assert!(policy.check_redirect("http://192.168.1.10/a", "http://192.168.1.10/b").is_ok());
  }

  #[test]
  fn a_redirect_target_is_held_to_the_same_policy_as_the_first_url() {
    let policy = FetchPolicy::source(false);
    let err = policy
      .check_redirect("https://example.com/a", "http://example.com/b")
      .expect_err("public http");
    assert_eq!(err.code, E_POLICY_INVALID);
  }

  #[test]
  fn a_manager_path_is_joined_below_the_root() {
    assert_eq!(
      join_child("https://example.com/r", "apk/index.json").expect("join"),
      "https://example.com/r/apk/index.json"
    );
    assert_eq!(
      join_child("https://example.com/r/", "apk/index.json").expect("join"),
      "https://example.com/r/apk/index.json"
    );
    assert_eq!(
      join_child("file:///srv/pica/r", "opkg/index.json").expect("join"),
      "file:///srv/pica/r/opkg/index.json"
    );
  }

  #[test]
  fn a_manager_path_may_not_leave_the_root() {
    for bad in ["../index.json", "/etc/index.json", "http://evil.invalid/x", "a/../../b.json"] {
      assert!(join_child("https://example.com/r", bad).is_err(), "{bad}");
    }
  }

  #[test]
  fn a_descriptor_path_resolves_beside_the_descriptor_not_inside_it() {
    assert_eq!(
      join_sibling("https://example.com/r/index.json", "apk/index.json").expect("sibling"),
      "https://example.com/r/apk/index.json"
    );
    assert_eq!(
      join_sibling("file:///srv/pica/r/index.json", "pica/index.json").expect("sibling"),
      "file:///srv/pica/r/pica/index.json"
    );
    assert_eq!(
      join_sibling("https://example.com/index.json", "opkg/index.json").expect("root"),
      "https://example.com/opkg/index.json"
    );
    assert!(join_sibling("https://example.com/r/index.json", "../index.json").is_err());
  }

  #[test]
  fn only_a_network_source_is_refused_a_local_download() {
    assert!(
      download_policy("file:///srv/pica/r", false).check("file:///srv/pica/p.pkg.tar.gz").is_ok()
    );
    // An index written before sources carried a root: not a remote source, so
    // its absolute file:// downloads keep working.
    assert!(download_policy("", false).check("file:///tmp/p.pkg.tar.gz").is_ok());
    assert!(download_policy("https://example.com/r", false).check("file:///etc/passwd").is_err());
  }

  #[test]
  fn a_lan_mirror_may_be_plain_http_without_a_global_switch() {
    let lan = download_policy("http://192.168.1.10/repo", false);
    assert!(lan.check("http://192.168.1.10/repo/p.pkg.tar.gz").is_ok());
    // The same policy still refuses a public host over plain HTTP.
    assert!(lan.check("http://example.com/p.pkg.tar.gz").is_err());
  }

  #[test]
  fn redirects_resolve_like_a_browser_would() {
    assert_eq!(
      resolve_location("https://a.example/r/index.json", "https://b.example/x.json").expect("abs"),
      "https://b.example/x.json"
    );
    assert_eq!(
      resolve_location("https://a.example/r/index.json", "/next/index.json").expect("abs path"),
      "https://a.example/next/index.json"
    );
    assert_eq!(
      resolve_location("https://a.example/r/index.json", "pkg/x.pkg.tar.gz").expect("relative"),
      "https://a.example/r/pkg/x.pkg.tar.gz"
    );
    assert_eq!(
      resolve_location("https://a.example/r/deep/index.json", "../apk/index.json").expect("dotdot"),
      "https://a.example/r/apk/index.json"
    );
  }

  #[test]
  fn a_protocol_relative_redirect_is_refused() {
    assert!(resolve_location("https://a.example/r/index.json", "//evil.invalid/x").is_err());
  }

  #[test]
  fn host_scopes_are_classified_without_resolving_names() {
    assert_eq!(host_scope("127.0.0.1:8080"), HostScope::Loopback);
    assert_eq!(host_scope("localhost"), HostScope::Loopback);
    assert_eq!(host_scope("10.1.2.3"), HostScope::Private);
    assert_eq!(host_scope("172.20.0.1"), HostScope::Private);
    assert_eq!(host_scope("172.32.0.1"), HostScope::Public);
    assert_eq!(host_scope("192.168.1.10"), HostScope::Private);
    assert_eq!(host_scope("169.254.1.1"), HostScope::LinkLocal);
    assert_eq!(host_scope("[::1]:8080"), HostScope::Loopback);
    assert_eq!(host_scope("[fd00::1]"), HostScope::Private);
    assert_eq!(host_scope("nas.lan"), HostScope::Public);
  }
}
