//! Which repositories an apk host really loads, and the index file each of them
//! needs.
//!
//! apk does not read one file. apk-tools 3.x — `OpenWrt` 25.12 ships
//! `apk-mbedtls` 3.0.2 — reads `/etc/apk/repositories` and then every `*.list`
//! file in `/etc/apk/repositories.d` and `/lib/apk/repositories.d`, the earlier
//! directory first and a file name taken by an earlier directory shadowing the
//! same name in a later one (`apk_dir_foreach_config_file` in `io.c`). `OpenWrt`
//! puts its feeds in `/etc/apk/repositories.d/distfeeds.list`, so a reader that
//! knows only about `/etc/apk/repositories` sees a host with no repositories at
//! all — and then answers "the feeds do not have this package" about a host
//! whose feeds have it.
//!
//! Lines are read the way `apk_repoparser_parse` reads them (`repoparser.c`,
//! same version):
//!
//! - a line is skipped when it is empty or its first column is `#`
//! - `set [-default] NAME=value` defines a variable; `APK_` names are reserved
//!   and a name starts with a letter and continues with letters, digits or `_`
//! - `ndx`, `v2` and `v3` name the index layout of the repository following
//!   them; without one, a URL ending in `.adb` or `.tar.gz` names its own index
//!   file and any other URL is a `v2` repository
//! - `@tag` tags the repository that follows it
//! - `${NAME}` in URLs, components and `set` values becomes the variable,
//!   `${NAME:N}` at most its first N characters, and an undefined variable is an
//!   error
//! - every word after the URL is a component: `.../main non-free` adds the
//!   repositories `.../main/non-free` and its sibling, each with its own index
//!
//! apk-tools only warns about a line it cannot read and drops the rest of that
//! file (`add_repos_from_file` turns a parse error of a file from a directory
//! into success). pica cannot know which feeds such a file still contributes, so
//! it reports the line and refuses to guess: [`read`] fails with the file, the
//! line number and what was wrong with it. The same goes for a configuration
//! that exists but cannot be read — apk-tools ignores an unreadable directory
//! (`if (ctx.atfd < 0) continue`), pica does not, because a feed list that may
//! be incomplete is exactly what this module exists to keep out of an answer.

use crate::app::{CliError, CliResult, E_CONFIG_INVALID, E_IO};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Display;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

/// The file that names apk's repositories.
pub(crate) const APK_REPOSITORIES: &str = "/etc/apk/repositories";
/// The directories apk scans for `*.list` repository files, in the order it
/// scans them: a name taken by an earlier directory shadows the same name in a
/// later one.
pub(crate) const APK_REPOSITORIES_DIRS: [&str; 2] =
  ["/etc/apk/repositories.d", "/lib/apk/repositories.d"];
/// The file that names this host's machine type, which apk puts into the index
/// URL of a repository that does not name its own index file.
pub(crate) const APK_ARCH: &str = "/etc/apk/arch";

/// How apk fetches the index of one repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
  /// The URL already names the index file (`v3`, `ndx`, or a URL ending in
  /// `.adb` or `.tar.gz`).
  Named,
  /// The index is `APKINDEX.tar.gz` in the machine type's directory under the
  /// URL.
  V2,
  /// The index is `Packages.adb` in the machine type's directory under the URL.
  V3,
}

/// Where apk reads the index of one repository from, which is also what decides
/// whether it has to be fetched at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IndexFile {
  /// A repository on this host: apk reads the index where it lies, and caches
  /// nothing for it (`apk_url_local_file` in `io.c`).
  Local(PathBuf),
  /// A repository apk fetches: the index is cached under this name in the
  /// directory apk caches in.
  Fetched(String),
}

impl IndexFile {
  /// Whether apk fetches this index, rather than reading it from this host.
  pub(crate) fn fetched(&self) -> bool {
    matches!(self, Self::Fetched(_))
  }
}

/// One repository apk loads an index for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Repository {
  /// The URL apk fetches the index from: the URL from the configuration file
  /// when that names the index file, and the URL with the machine type and the
  /// index file name appended otherwise (`add_repository_component` in
  /// `database.c`).
  pub(crate) url_index: String,
  /// [`Repository::url_index`] without credentials, for messages.
  pub(crate) printable: String,
  /// Where the index of this repository is. The name of a fetched index is a
  /// digest of the index URL, which is what ties a cached file to the
  /// repository that asked for it.
  pub(crate) index: IndexFile,
  /// The configuration file that named this repository.
  pub(crate) source: PathBuf,
}

/// The repositories apk would load on this host.
#[derive(Debug, Clone, Default)]
pub(crate) struct Configuration {
  /// Every repository apk loads, in configuration order, without duplicates.
  pub(crate) repositories: Vec<Repository>,
  /// The configuration files that exist and were read.
  pub(crate) files: Vec<PathBuf>,
}

impl Configuration {
  /// Whether apk would load no repository at all. A host without repositories
  /// has no feeds to answer from, which is a different thing from a host whose
  /// feeds cannot be read.
  pub(crate) fn is_empty(&self) -> bool {
    self.repositories.is_empty()
  }
}

/// Reads the repositories apk would load, the way apk loads them.
///
/// `files` are the repository files apk reads in order, `dirs` the directories
/// it then scans for `*.list` files, and `arch_file` the file naming the machine
/// type. A file or directory that does not exist is not an error — a host is
/// allowed to have no feeds — but one that cannot be read, and a line that
/// cannot be read, is.
pub(crate) fn read(
  files: &[PathBuf],
  dirs: &[PathBuf],
  arch_file: &Path,
) -> CliResult<Configuration> {
  let mut parser = Parser::new(machine_type(arch_file)?, arch_file);
  let mut configuration = Configuration::default();
  for path in configuration_files(files, dirs)? {
    let text = match fs::read_to_string(&path) {
      Ok(text) => text,
      Err(err) if err.kind() == ErrorKind::NotFound => continue,
      Err(err) => {
        return Err(CliError::new(E_IO, format!("read {} failed: {err}", path.display())));
      }
    };
    parser.parse_file(&path, &text)?;
    configuration.files.push(path);
  }
  configuration.repositories = parser.repositories;
  Ok(configuration)
}

/// The files apk reads, in the order it reads them: the repository files first,
/// then the `*.list` files of the configuration directories, sorted by name,
/// where a name taken by an earlier directory hides the same name in a later one
/// (`apk_dir_config_file_amend`, `io.c`).
// `file_not_dot_list` compares the suffix as written: a file named
// `feeds.LIST` is not one apk reads, and should not become one here either.
#[allow(
  clippy::case_sensitive_file_extension_comparisons,
  reason = "apk compares the suffix as written"
)]
fn configuration_files(files: &[PathBuf], dirs: &[PathBuf]) -> CliResult<Vec<PathBuf>> {
  let mut shadowed: BTreeMap<String, PathBuf> = BTreeMap::new();
  for dir in dirs {
    let entries = match fs::read_dir(dir) {
      Ok(entries) => entries,
      Err(err) if err.kind() == ErrorKind::NotFound => continue,
      Err(err) => {
        return Err(CliError::new(E_IO, format!("read directory {} failed: {err}", dir.display())));
      }
    };
    for entry in entries {
      let entry = entry.map_err(|err| {
        CliError::new(E_IO, format!("read directory {} failed: {err}", dir.display()))
      })?;
      let name = entry.file_name();
      let Some(name) = name.to_str() else { continue };
      // apk reads the files that are not hidden and end in `.list`
      // (`file_not_dot_list`), and nothing else in there.
      if name.starts_with('.') || !name.ends_with(".list") {
        continue;
      }
      shadowed.entry(name.to_string()).or_insert_with(|| entry.path());
    }
  }
  Ok(files.iter().cloned().chain(shadowed.into_values()).collect())
}

/// The machine type apk would use: the first line of its `arch` file, which is
/// the `APK_ARCH` variable apk predefines and does not let a configuration
/// change.
fn machine_type(path: &Path) -> CliResult<Option<String>> {
  match fs::read_to_string(path) {
    Ok(text) => Ok(text.lines().map(str::trim).find(|line| !line.is_empty()).map(str::to_string)),
    Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
    Err(err) => Err(CliError::new(E_IO, format!("read {} failed: {err}", path.display()))),
  }
}

struct Parser {
  arch: Option<String>,
  arch_file: PathBuf,
  variables: BTreeMap<String, String>,
  repositories: Vec<Repository>,
  seen: BTreeSet<String>,
  file: PathBuf,
  line: usize,
}

impl Parser {
  fn new(arch: Option<String>, arch_file: &Path) -> Self {
    let mut variables = BTreeMap::new();
    if let Some(arch) = &arch {
      variables.insert("APK_ARCH".to_string(), arch.clone());
    }
    Self {
      arch,
      arch_file: arch_file.to_path_buf(),
      variables,
      repositories: Vec::new(),
      seen: BTreeSet::new(),
      file: PathBuf::new(),
      line: 0,
    }
  }

  fn parse_file(&mut self, path: &Path, text: &str) -> CliResult<()> {
    self.file = path.to_path_buf();
    self.line = 0;
    for line in text.lines() {
      self.line += 1;
      self.parse_line(line)?;
    }
    Ok(())
  }

  // apk asks whether the URL as written ends in `.adb` or `.tar.gz`: a URL
  // written `packages.ADB` is a base URL for it, and folding the case here
  // would make pica read a repository apk does not.
  #[allow(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "apk compares the suffix as written"
  )]
  fn parse_line(&mut self, line: &str) -> CliResult<()> {
    // Only a line whose first column is `#` is a comment: an indented `#` is
    // read as a keyword and reported as one below.
    if line.is_empty() || line.starts_with('#') {
      return Ok(());
    }
    let mut words = Words::new(line);
    let Some(first) = words.next() else {
      return Err(self.invalid("no keyword or URL"));
    };
    let mut named: Option<Layout> = None;
    let url = if keyword(first) {
      match first {
        // `set` takes the rest of the line, not a URL.
        "set" => return self.parse_set(words.rest()),
        "ndx" => named = Some(Layout::Named),
        "v2" => named = Some(Layout::V2),
        "v3" => named = Some(Layout::V3),
        other => return Err(self.invalid(format!("unrecognized keyword: {other}"))),
      }
      words.next()
    } else {
      Some(first)
    };
    let Some(url) = url else {
      return Err(self.invalid("no URL"));
    };
    // `@tag URL` tags the repository. The tag groups repositories for
    // installation, it does not change which index apk fetches, so it does not
    // reach the index check.
    let url = if url.starts_with('@') { words.next() } else { Some(url) };
    let Some(url) = url else {
      return Err(self.invalid("no URL"));
    };
    // apk decides the layout from the URL as written and expands variables only
    // afterwards.
    let layout = named.unwrap_or(if url.ends_with(".adb") || url.ends_with(".tar.gz") {
      Layout::Named
    } else {
      Layout::V2
    });
    let url = self.expand(url)?;
    if !is_url(&url) {
      return Err(self.invalid(format!("invalid URL: {url}")));
    }
    let base = url.trim_end_matches('/').to_string();
    let components = self.expand(words.rest())?;
    if components.is_empty() {
      return self.repository(&base, layout);
    }
    // apk expands the whole rest of the line before splitting it into
    // components, so a variable whose value contains separators adds several
    // components.
    for component in components.split(component_separator).filter(|word| !word.is_empty()) {
      self.repository(&format!("{base}/{component}"), layout)?;
    }
    Ok(())
  }

  /// `set [-default] NAME=value`: defines a variable, `-default` leaving an
  /// existing definition alone.
  fn parse_set(&mut self, text: &str) -> CliResult<()> {
    let mut words = Words::new(text);
    let mut overwrite = true;
    while words.rest().starts_with('-') {
      let Some(option) = words.next() else { break };
      if option == "-default" {
        overwrite = false;
      } else {
        return Err(self.invalid(format!("invalid option: {option}")));
      }
    }
    let definition = words.rest();
    let Some((name, value)) = definition.split_once('=') else {
      return Err(self.invalid(format!("invalid variable definition: {definition}")));
    };
    // The name is checked before the value is expanded, the way
    // `apk_repoparser_parse_set` checks it: `APK_` is apk's own namespace.
    if name.starts_with("APK_") || !valid_variable_name(name) {
      return Err(self.invalid(format!("invalid variable definition: {definition}")));
    }
    let value = self.expand(value)?;
    if !overwrite && self.variables.contains_key(name) {
      return Ok(());
    }
    self.variables.insert(name.to_string(), value);
    Ok(())
  }

  /// Adds one repository, once.
  fn repository(&mut self, url: &str, layout: Layout) -> CliResult<()> {
    let url_index = match layout {
      Layout::Named => url.to_string(),
      Layout::V2 | Layout::V3 => {
        let Some(arch) = &self.arch else {
          return Err(self.invalid(format!(
            "cannot tell which index {url} needs: {} does not name a machine type",
            self.arch_file.display()
          )));
        };
        let index = if layout == Layout::V2 { "APKINDEX.tar.gz" } else { "Packages.adb" };
        format!("{url}/{arch}/{index}")
      }
    };
    // The same repository named twice is one index to fetch, which is also how
    // apk counts them: it merges the entries that share an index URL.
    if !self.seen.insert(url_index.clone()) {
      return Ok(());
    }
    let index = match local_file(&url_index) {
      Some(path) => IndexFile::Local(local_path(path)),
      None => IndexFile::Fetched(format!("APKINDEX.{}.tar.gz", digest(&url_index))),
    };
    self.repositories.push(Repository {
      printable: sanitize(&url_index),
      url_index,
      index,
      source: self.file.clone(),
    });
    Ok(())
  }

  /// Replaces `${NAME}` the way `apk_blob_subst` does: by the value of the
  /// variable, and `${NAME:N}` by at most its first N characters. An undefined
  /// variable is an error — apk warns about it and drops the rest of the file.
  fn expand(&self, text: &str) -> CliResult<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((prefix, after)) = rest.split_once("${") {
      out.push_str(prefix);
      let Some((key, after)) = after.split_once('}') else {
        return Err(self.invalid(format!("unterminated variable: {text}")));
      };
      // A length is a run of digits, and all of them have to be digits
      // (`apk_blob_pull_uint`): `${NAME:8x}` is not a length at all, and
      // `${NAME:}` is none of the value. apk reads the length before it looks
      // the variable up, so a bad length is a bad length whether or not the
      // variable exists. And apk counts bytes where pica counts characters,
      // which differ only for a value that is not ASCII.
      let (name, count) = match key.split_once(':') {
        None => (key, None),
        Some((name, "")) => (name, Some(0)),
        Some((name, limit)) => match limit.parse::<usize>() {
          Ok(count) if limit.bytes().all(|byte| byte.is_ascii_digit()) => (name, Some(count)),
          _ => return Err(self.invalid(format!("invalid variable length: {key}"))),
        },
      };
      let Some(value) = self.variables.get(name) else {
        return Err(self.invalid(format!("undefined variable: {name}")));
      };
      match count {
        Some(count) => out.extend(value.chars().take(count)),
        None => out.push_str(value),
      }
      rest = after;
    }
    out.push_str(rest);
    Ok(out)
  }

  fn invalid(&self, message: impl Display) -> CliError {
    CliError::new(E_CONFIG_INVALID, format!("{}:{}: {message}", self.file.display(), self.line))
  }
}

/// The words of a line, read the way `get_word` reads them: a word runs up to
/// the next separator and the separators after it are consumed with it. The
/// first word therefore has to start in the first column, and [`Words::rest`] —
/// what apk puts in `components` — never starts with a separator.
struct Words<'a> {
  rest: &'a str,
}

impl<'a> Words<'a> {
  fn new(line: &'a str) -> Self {
    Self { rest: line }
  }

  fn next(&mut self) -> Option<&'a str> {
    let end = self.rest.find(separator).unwrap_or(self.rest.len());
    let word = &self.rest[..end];
    self.rest = self.rest[end..].trim_start_matches(separator);
    if word.is_empty() { None } else { Some(word) }
  }

  fn rest(&self) -> &'a str {
    self.rest
  }
}

/// `APK_CTYPE_DEPENDENCY_SEPARATOR`, which is what apk splits the components of
/// a repository line on: a space, and a newline a line cannot contain. A tab
/// separates words but not components, so a tab among the components stays in
/// the URL apk would fetch.
fn component_separator(c: char) -> bool {
  matches!(c, ' ' | '\n')
}

/// `APK_CTYPE_REPOSITORY_SEPARATOR`. apk splits its configuration files into
/// lines on `\n` alone, so a file with `\r\n` line endings leaves the `\r` in
/// the line; treating it as a separator reads such a file the way it was meant.
fn separator(c: char) -> bool {
  matches!(c, ' ' | '\t' | '\r' | '\n')
}

/// Whether a word is a URL rather than an apk keyword: a URL either starts with
/// `/` or contains `://`.
fn is_url(word: &str) -> bool {
  word.starts_with('/') || word.contains("://")
}

fn keyword(word: &str) -> bool {
  !word.starts_with('@') && !is_url(word)
}

/// Whether a word is a variable name: apk requires the first character to be a
/// letter and the rest to be letters, digits or `_`.
fn valid_variable_name(name: &str) -> bool {
  let mut chars = name.chars();
  match chars.next() {
    Some(first) if first.is_ascii_alphabetic() => {}
    _ => return false,
  }
  chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The file apk reads this index from, for a repository that is on this host,
/// and `None` for one it fetches (`apk_url_local_file` in `io.c`).
///
/// apk goes by the first colon of the URL and gives up after ten characters, so
/// a scheme longer than that is a path, and `file:nu.nl` is a path because the
/// colon is not followed by `//`. Only a URL that starts with `/` would not
/// reach the loop at all. `test:` is how `apk-tools`' own test suite names a
/// repository, and it is fetched like a network one.
fn local_file(url: &str) -> Option<&str> {
  if url.starts_with("test:") {
    return None;
  }
  if url.len() < 4 || url.starts_with('/') {
    return Some(url);
  }
  if let Some(path) = url.strip_prefix("file:") {
    return Some(path);
  }
  // The first colon decides, and only a colon with `//` after it makes the URL
  // one for the network.
  let scanned = url.len().min(10) - 2;
  if let Some((at, _)) = url.char_indices().take(scanned).find(|(_, c)| *c == ':') {
    return if url[at..].starts_with("://") { None } else { Some(url) };
  }
  Some(url)
}

/// The path of a local repository index, as a path. apk strips the scheme only
/// (`file:/srv/packages` → `/srv/packages`), which leaves `file:///srv/packages`
/// with three leading slashes: the same file, but read as a mistake in a
/// message.
fn local_path(path: &str) -> PathBuf {
  match path.strip_prefix('/') {
    Some(rest) => PathBuf::from(format!("/{}", rest.trim_start_matches('/'))),
    None => PathBuf::from(path),
  }
}

/// The digest apk puts in the name of a cached index: the first four bytes of
/// the SHA-256 of the index URL, as lower case hex
/// (`apk_repo_subst(db->default_cachename_spec, url_index)` in `database.c`,
/// where the cache name is `APKINDEX.%{hash:8}.tar.gz` of the index URL).
fn digest(url: &str) -> String {
  const HEX: &[u8; 16] = b"0123456789abcdef";
  let sum = Sha256::digest(url.as_bytes());
  let mut out = String::with_capacity(8);
  for byte in sum.iter().take(4) {
    out.push(HEX[(byte >> 4) as usize] as char);
    out.push(HEX[(byte & 0xf) as usize] as char);
  }
  out
}

/// The URL without the password it carries, so a message can name a feed
/// without printing the credentials that reach it.
fn sanitize(url: &str) -> String {
  let Some((scheme, rest)) = url.split_once("://") else {
    return url.to_string();
  };
  let Some((userinfo, host)) = rest.split_once('@') else {
    return url.to_string();
  };
  if userinfo.contains('/') {
    // the `@` was in the path, not in credentials
    return url.to_string();
  }
  match userinfo.split_once(':') {
    Some((user, _)) => format!("{scheme}://{user}:***@{host}"),
    None => url.to_string(),
  }
}

#[cfg(test)]
mod tests {
  use super::{
    APK_REPOSITORIES, APK_REPOSITORIES_DIRS, Configuration, IndexFile, Repository, read,
  };
  use crate::app::{CliResult, E_CONFIG_INVALID, E_IO};
  use pretty_assertions::assert_eq;
  use std::fs;
  use std::path::PathBuf;

  /// The eight feeds `OpenWrt` 25.12 writes to
  /// `/etc/apk/repositories.d/distfeeds.list`, as shipped in
  /// `openwrt-25.12.0-x86-64-rootfs.tar.gz`.
  const DISTFEEDS: &str = "\
https://downloads.openwrt.org/releases/25.12.0/targets/x86/64/packages/packages.adb
https://downloads.openwrt.org/releases/25.12.0/packages/x86_64/base/packages.adb
https://downloads.openwrt.org/releases/25.12.0/targets/x86/64/kmods/6.12.71-1-dad96e0b324e75e311f44d061391d208/packages.adb
https://downloads.openwrt.org/releases/25.12.0/packages/x86_64/luci/packages.adb
https://downloads.openwrt.org/releases/25.12.0/packages/x86_64/packages/packages.adb
https://downloads.openwrt.org/releases/25.12.0/packages/x86_64/routing/packages.adb
https://downloads.openwrt.org/releases/25.12.0/packages/x86_64/telephony/packages.adb
https://downloads.openwrt.org/releases/25.12.0/packages/x86_64/video/packages.adb
";

  /// The comments `OpenWrt` 25.12 ships in
  /// `/etc/apk/repositories.d/customfeeds.list`.
  const CUSTOMFEEDS: &str = "\
# add your custom package feeds here
#
# http://www.example.com/path/to/files/packages.adb
";

  /// A host whose `/etc/apk` the test decides.
  struct Host {
    root: tempfile::TempDir,
  }

  impl Host {
    fn new() -> Self {
      Self { root: tempfile::tempdir().expect("create temp dir") }
    }

    /// The `OpenWrt` 25.12 layout: the feeds in the configuration directory and
    /// the machine type next to them, and no `/etc/apk/repositories`.
    fn openwrt() -> Self {
      let host = Self::new();
      host.write("etc/apk/arch", "x86_64\n");
      host.write("etc/apk/repositories.d/distfeeds.list", DISTFEEDS);
      host.write("etc/apk/repositories.d/customfeeds.list", CUSTOMFEEDS);
      host
    }

    fn path(&self, path: &str) -> PathBuf {
      self.root.path().join(path.trim_start_matches('/'))
    }

    fn write(&self, path: &str, text: &str) -> PathBuf {
      let path = self.path(path);
      fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
      fs::write(&path, text).expect("write file");
      path
    }

    fn read(&self) -> CliResult<Configuration> {
      let files = vec![self.path(APK_REPOSITORIES)];
      let dirs = APK_REPOSITORIES_DIRS.iter().map(|dir| self.path(dir)).collect::<Vec<_>>();
      read(&files, &dirs, &self.path("/etc/apk/arch"))
    }
  }

  fn indexes(configuration: &Configuration) -> Vec<&str> {
    configuration.repositories.iter().map(|repository| repository.url_index.as_str()).collect()
  }

  /// Where the index apk needs for this repository is: the name apk caches a
  /// fetched index under, or the file apk reads a local one from.
  fn index_of(repository: &Repository) -> String {
    match &repository.index {
      IndexFile::Fetched(name) => name.clone(),
      IndexFile::Local(path) => path.display().to_string(),
    }
  }

  #[test]
  fn the_openwrt_feeds_are_read() {
    let host = Host::openwrt();

    let configuration = host.read().expect("read the configuration");

    // Every `packages.adb` URL names its own index, so nothing is appended to
    // it and the machine type is not needed to read this host.
    assert_eq!(indexes(&configuration), DISTFEEDS.lines().collect::<Vec<_>>());
    assert!(configuration.repositories.iter().all(|fed| fed.index.fetched()));
    // The name of a cached index is a digest of the index URL and nothing else,
    // which is what lets pica tell which feed is the one without an index. The
    // digest here is what `shasum -a 256` says about that URL, so the cache file
    // apk looks for on an OpenWrt 25.12 host is spelled out.
    assert_eq!(index_of(&configuration.repositories[0]), "APKINDEX.d96da355.tar.gz");
    assert_eq!(configuration.files.len(), 2);
  }

  #[test]
  fn a_missing_configuration_is_a_host_without_feeds() {
    let host = Host::new();

    let configuration = host.read().expect("read the configuration");

    assert!(configuration.is_empty());
    assert_eq!(configuration.files, Vec::<std::path::PathBuf>::new());
  }

  #[test]
  fn the_legacy_repository_file_is_still_read() {
    let host = Host::new();
    host.write("etc/apk/arch", "x86_64\n");
    host.write(APK_REPOSITORIES, "# alpine\nhttps://dl-cdn.alpinelinux.org/alpine/v3.20/main\n");

    let configuration = host.read().expect("read the configuration");

    // A plain URL is a `v2` repository: `APKINDEX.tar.gz` under the URL, in the
    // machine type's directory.
    assert_eq!(
      indexes(&configuration),
      ["https://dl-cdn.alpinelinux.org/alpine/v3.20/main/x86_64/APKINDEX.tar.gz"]
    );
  }

  #[test]
  fn a_base_url_needs_the_machine_type() {
    let host = Host::new();
    host.write(APK_REPOSITORIES, "https://example.test/main\n");

    let err = host.read().expect_err("no machine type");

    assert_eq!(err.code, E_CONFIG_INVALID);
    assert!(err.message.contains("does not name a machine type"), "{}", err.message);
  }

  #[test]
  fn a_repository_named_twice_is_one_index() {
    let host = Host::new();
    host.write("etc/apk/arch", "x86_64\n");
    host.write(
      APK_REPOSITORIES,
      "https://example.test/main\n@tag https://example.test/main\nv2 https://example.test/main/\n",
    );

    let configuration = host.read().expect("read the configuration");

    assert_eq!(indexes(&configuration), ["https://example.test/main/x86_64/APKINDEX.tar.gz"]);
  }

  #[test]
  fn local_repositories_are_read_where_they_lie() {
    let host = Host::new();
    host.write("etc/apk/arch", "x86_64\n");
    host.write(APK_REPOSITORIES, "/srv/packages\nfile:///srv/more\nhttps://example.test/main\n");

    let configuration = host.read().expect("read the configuration");

    // A URL is what apk prints and hashes, so the `file://` stays in it …
    assert_eq!(
      indexes(&configuration),
      [
        "/srv/packages/x86_64/APKINDEX.tar.gz",
        "file:///srv/more/x86_64/APKINDEX.tar.gz",
        "https://example.test/main/x86_64/APKINDEX.tar.gz",
      ]
    );
    // … while the path apk opens is the one without the scheme, and a
    // repository read from a file is cached nowhere.
    let found: Vec<String> = configuration.repositories.iter().map(index_of).collect();
    assert_eq!(
      found,
      [
        "/srv/packages/x86_64/APKINDEX.tar.gz",
        "/srv/more/x86_64/APKINDEX.tar.gz",
        // `shasum -a 256` of the index URL of the third repository
        "APKINDEX.3521cdee.tar.gz",
      ]
    );
    let fetched: Vec<bool> =
      configuration.repositories.iter().map(|repository| repository.index.fetched()).collect();
    assert_eq!(fetched, [false, false, true]);
  }

  #[test]
  fn credentials_are_not_printed() {
    let host = Host::new();
    host.write(APK_REPOSITORIES, "https://user:secret@example.test/packages.adb\n");

    let configuration = host.read().expect("read the configuration");

    assert_eq!(
      configuration.repositories[0].printable,
      "https://user:***@example.test/packages.adb"
    );
    assert_eq!(indexes(&configuration), ["https://user:secret@example.test/packages.adb"]);
  }

  #[test]
  fn an_earlier_directory_shadows_a_later_one_by_name() {
    let host = Host::new();
    host.write("etc/apk/repositories.d/feeds.list", "https://etc.test/packages.adb\n");
    host.write("etc/apk/repositories.d/other.list", "https://etc.test/other/packages.adb\n");
    host.write("lib/apk/repositories.d/feeds.list", "https://lib.test/feeds/packages.adb\n");
    host.write("lib/apk/repositories.d/lib.list", "https://lib.test/lib/packages.adb\n");
    // apk reads the files that are not hidden and end in `.list`, nothing else
    host.write("lib/apk/repositories.d/.hidden.list", "https://lib.test/hidden/packages.adb\n");
    host.write("lib/apk/repositories.d/ignored.conf", "https://lib.test/conf/packages.adb\n");

    let configuration = host.read().expect("read the configuration");

    // read in name order, `lib/apk/repositories.d/feeds.list` never read: the
    // name belongs to the directory apk scanned first
    assert_eq!(
      indexes(&configuration),
      [
        "https://etc.test/packages.adb",
        "https://lib.test/lib/packages.adb",
        "https://etc.test/other/packages.adb",
      ]
    );
  }

  #[test]
  fn tags_components_and_layouts_are_read_like_apk() {
    // The cases of `test/unit/repoparser_test.c` in apk-tools 3.0.2, which
    // prints the index file apk reads from every line.
    let host = Host::new();
    host.write("etc/apk/arch", "x86_64\n");
    host.write(
      APK_REPOSITORIES,
      "# test data\n\
       http://example.com/edge/main\n\
       @tag http://example.com/edge/testing\n\
       ndx http://example.com/repo/Packages.adb\n\
       v2 http://example.com/main\n\
       v3 http://example.com/main\n\
       v3 @tag http://example.com/testing\n\
       http://example.com/ main community\n",
    );

    let configuration = host.read().expect("read the configuration");

    assert_eq!(
      indexes(&configuration),
      [
        "http://example.com/edge/main/x86_64/APKINDEX.tar.gz",
        "http://example.com/edge/testing/x86_64/APKINDEX.tar.gz",
        "http://example.com/repo/Packages.adb",
        "http://example.com/main/x86_64/APKINDEX.tar.gz",
        "http://example.com/main/x86_64/Packages.adb",
        "http://example.com/testing/x86_64/Packages.adb",
        "http://example.com/community/x86_64/APKINDEX.tar.gz",
      ]
    );
    // the `main` component of the last line is the `v2` repository of the fourth
    // line: one index, fetched once
  }

  #[test]
  fn a_repository_named_twice_is_one_index_from_any_directory() {
    let host = Host::new();
    host.write("etc/apk/arch", "x86_64\n");
    host.write(APK_REPOSITORIES, "https://example.test/main\n");
    host.write("etc/apk/repositories.d/feeds.list", "https://example.test/main\n");

    let configuration = host.read().expect("read the configuration");

    assert_eq!(indexes(&configuration), ["https://example.test/main/x86_64/APKINDEX.tar.gz"]);
  }

  #[test]
  fn variables_are_expanded() {
    let host = Host::new();
    host.write("etc/apk/arch", "x86_64\n");
    host.write(
      APK_REPOSITORIES,
      // `-default` leaves a variable that is already defined alone, a
      // definition without it takes over
      "set -default mirror=alpine.org\n\
       http://${mirror}/main\n\
       set mirror=example.com\n\
       http://${mirror}/main\n\
       set -default mirror=nowhere.test\n\
       http://${mirror}/main\n\
       set comp=main community\n\
       http://${mirror}/ ${comp}\n\
       set defined=once\n\
       http://${defined:4}/${APK_ARCH}\n",
    );

    let configuration = host.read().expect("read the configuration");

    assert_eq!(
      indexes(&configuration),
      [
        // the first definition of `mirror`, then the definition that took it
        // over: the third line names the repository of the second one again,
        // which is one index and one fetch
        "http://alpine.org/main/x86_64/APKINDEX.tar.gz",
        "http://example.com/main/x86_64/APKINDEX.tar.gz",
        // a variable whose value holds two words adds two components
        "http://example.com/community/x86_64/APKINDEX.tar.gz",
        // `${defined:4}` is the first four characters of `once`, and
        // `${APK_ARCH}` the machine type of this host
        "http://once/x86_64/x86_64/APKINDEX.tar.gz",
      ]
    );
  }

  #[test]
  fn a_reserved_variable_name_is_reported() {
    let host = Host::new();
    host.write("etc/apk/arch", "x86_64\n");
    host.write(APK_REPOSITORIES, "set APK_ARCH=x86\n");

    let err = host.read().expect_err("a reserved name");

    assert_eq!(err.code, E_CONFIG_INVALID);
    assert!(err.message.contains("invalid variable definition: APK_ARCH=x86"), "{}", err.message);
  }

  #[test]
  fn a_line_that_cannot_be_read_is_reported() {
    let cases = [
      // a comment is a line whose *first* column is `#`, so an indented one is a
      // line apk cannot read and does not read as a comment
      ("  # indented\n", "repositories:1: no keyword or URL"),
      ("main\n", "repositories:1: unrecognized keyword: main"),
      ("   \n", "repositories:1: no keyword or URL"),
      ("@tag\n", "repositories:1: no URL"),
      ("v2 foobar main\n", "repositories:1: invalid URL: foobar"),
      ("http://${undefined}/main\n", "repositories:1: undefined variable: undefined"),
      ("http://${unterminated/main\n", "repositories:1: unterminated variable"),
      ("http://${a:8x}/main\n", "repositories:1: invalid variable length: a:8x"),
      ("set -unknown mirror=x\n", "repositories:1: invalid option: -unknown"),
      ("set var-foo=bad\n", "repositories:1: invalid variable definition: var-foo=bad"),
      ("set\n", "repositories:1: invalid variable definition: "),
    ];
    for (text, expected) in cases {
      let host = Host::new();
      host.write("etc/apk/arch", "x86_64\n");
      host.write(APK_REPOSITORIES, text);

      let err = host.read().expect_err(text);

      assert_eq!(err.code, E_CONFIG_INVALID, "{text}");
      assert!(err.message.contains(expected), "{text}: {}", err.message);
    }
  }

  #[test]
  fn a_configuration_that_cannot_be_read_is_an_error() {
    let host = Host::new();
    // a directory where apk expects a file: apk-tools reports the read error of
    // its own repository file, and so does pica
    fs::create_dir_all(host.path(APK_REPOSITORIES)).expect("create dir");

    let err = host.read().expect_err("unreadable configuration");

    assert_eq!(err.code, E_IO);
    assert!(err.message.starts_with("read "), "{}", err.message);
  }
}
