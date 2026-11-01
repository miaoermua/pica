//! The text Pica prints to a person.
//!
//! One table per language, keyed by [`Key`], filled by [`Lang::text`] and
//! [`Lang::args`]. English is the fallback for a language that does not know a
//! key, and it is the default language, so a machine that never configures one
//! sees exactly the output it saw before this module existed.
//!
//! What is deliberately *not* in here:
//!
//! - `-V` output, JSON payloads (the install report included), the
//!   `-Q`/`-Qi`/`-Ql`/`-Si` field labels and error codes. Those are interfaces
//!   other programs read, so they stay in one shape whatever the language is.
//!   `--help` is for people and is translated; its command column is not.
//! - Package names, command names, config keys, protocol fields and paths.
//! - The words a third-party tool said. A message may be wrapped in a
//!   translated sentence, but the original diagnostic is kept verbatim.
//!
//! Code that has an [`crate::app::App`] uses its `text`/`args`; code that does
//! not (the fetchers, the backends, the source checks) uses [`tr`], which reads
//! the language the run made current. A test in this module keeps literal
//! sentences out of the code: see `no_sentence_is_written_outside_this_table`.
//!
//! Adding a language means adding a `Lang` variant, a table, and a code in
//! [`Lang::code`]. Adding text means adding a `Key` — the English match is
//! exhaustive, so a missing English string is a compile error rather than an
//! empty line on somebody's terminal.

/// A language Pica can speak.
///
/// The variant order is the order of the setup menu, and the default is the
/// language every existing release printed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Lang {
  /// English, `en-US`.
  #[default]
  EnUs,
  /// Simplified Chinese, `zh-CN`.
  ZhCn,
}

impl Lang {
  /// Every language the setup menu offers, in menu order.
  pub(crate) const ALL: [Self; 2] = [Self::ZhCn, Self::EnUs];

  /// Reads a language code or the name of a language.
  ///
  /// Region and hyphen/underscore variants are accepted (`zh-cn`, `zh_CN`,
  /// `en`), because a hand-written config should not have to guess the exact
  /// spelling. Anything else is `None`, which callers turn into the default
  /// rather than an error: an unknown language must not make Pica unusable.
  pub(crate) fn parse(text: &str) -> Option<Self> {
    match text.trim().to_ascii_lowercase().as_str() {
      "en" | "en-us" | "en_us" | "en-gb" | "english" => Some(Self::EnUs),
      "zh" | "zh-cn" | "zh_cn" | "zh-hans" | "zh-hans-cn" | "zh-chs" | "chinese" => {
        Some(Self::ZhCn)
      }
      _ => None,
    }
  }

  /// The stable identifier this language is saved and printed as.
  pub(crate) fn code(self) -> &'static str {
    match self {
      Self::EnUs => "en-US",
      Self::ZhCn => "zh-CN",
    }
  }

  /// The name of the language in the language itself, for the setup menu.
  pub(crate) fn native_name(self) -> &'static str {
    match self {
      Self::EnUs => "English",
      Self::ZhCn => "简体中文",
    }
  }

  /// The text for `key`. Both tables are exhaustive, so a key always has a
  /// translation.
  pub(crate) fn text(self, key: Key) -> &'static str {
    match self {
      Self::EnUs => english(key),
      Self::ZhCn => chinese(key),
    }
  }

  /// [`Lang::text`] with `{0}`, `{1}`... replaced by `args`.
  pub(crate) fn args(self, key: Key, args: &[&str]) -> String {
    fill(self.text(key), args)
  }

  /// The language stored in a configuration document, or the default.
  ///
  /// `pica.json` keeps this in a top-level `language` field. The older `i18n`
  /// key is *not* read here: it names the `LuCI` language package to install and
  /// changing what `-i` installs because the UI language changed would be a
  /// silent change of behaviour.
  #[must_use]
  pub(crate) fn from_config(conf: &serde_json::Value) -> Self {
    conf
      .get("language")
      .and_then(serde_json::Value::as_str)
      .and_then(Self::parse)
      .unwrap_or_default()
  }
}

thread_local! {
  /// The language of this run, for code that has no `App` to ask: the
  /// fetchers, the backends and the source checks. [`Lang::make_current`]
  /// sets it when the run's language is decided or changed.
  ///
  /// Per thread rather than global, so tests that run side by side with
  /// different languages cannot see each other's choice; pica itself prints
  /// from one thread.
  static CURRENT: std::cell::Cell<Lang> = const { std::cell::Cell::new(Lang::EnUs) };
}

impl Lang {
  /// Makes `self` the language [`tr`] speaks on this thread.
  pub(crate) fn make_current(self) {
    CURRENT.with(|current| current.set(self));
  }
}

/// The text for `key` in the run's language, with `{0}`, `{1}`... replaced by
/// `args`. Anything that displays can be an argument, so a path, an error or a
/// number goes in as it is.
pub(crate) fn tr(key: Key, args: &[&dyn std::fmt::Display]) -> String {
  let args: Vec<String> = args.iter().map(ToString::to_string).collect();
  let args: Vec<&str> = args.iter().map(String::as_str).collect();
  CURRENT.with(std::cell::Cell::get).args(key, &args)
}

/// Replace `{0}`, `{1}`… in `template` with `args`.
///
/// A placeholder with no argument, or a `{` that does not start a placeholder,
/// is left as written: the goal is to never lose text, not to be a formatter.
fn fill(template: &str, args: &[&str]) -> String {
  let mut out = String::with_capacity(template.len() + 16);
  let mut rest = template;

  while let Some(start) = rest.find('{') {
    out.push_str(&rest[..start]);

    let after = &rest[start + 1..];
    let Some(end) = after.find('}') else {
      out.push_str(&rest[start..]);
      return out;
    };

    let token = &after[..end];
    let filled = token.parse::<usize>().ok().and_then(|index| args.get(index));
    if let Some(value) = filled {
      out.push_str(value);
      rest = &after[end + 1..];
    } else {
      out.push('{');
      rest = after;
    }
  }

  out.push_str(rest);
  out
}

/// One piece of text Pica prints. The variant names are the keys; the English
/// table is the source of truth for the wording and for the argument count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Key {
  // ---- setup -------------------------------------------------------------
  SetupTitle,
  SetupNeedsTerminal,
  SetupConfUnreadable,
  SetupNotAnObject,
  SetupCancelled,
  SetupInvalidChoice,
  SetupLanguagePrompt,
  SetupSources,
  SetupSourceLine,
  SetupSourcePrompt,
  SetupSourceOfficial,
  SetupSourceCustom,
  SetupSourceKeep,
  SetupAllowHttp,
  SetupYes,
  SetupNo,
  SetupSourceUrl,
  SetupUrlEmpty,
  SetupOfficialMissing,
  SetupNoCommonBackend,
  SetupBackends,
  SetupSaved,
  SetupComplete,
  SetupNextSteps,
  SetupNotJson,
  // ---- sync --------------------------------------------------------------
  SyncStarted,
  SyncDownloading,
  SyncUpdated,
  SyncPackageSkipped,
  SyncNoRepos,
  SyncReposNotArray,
  SyncRepoMissingName,
  SyncRepoMissingUrl,
  SyncBackendsMissing,
  SyncBackendsNotArray,
  SyncUnsupportedBackend,
  SyncBackendsEmpty,
  SyncRootRejected,
  SyncFetchFailed,
  SyncNotUtf8,
  SyncEntryInvalid,
  SyncMissingBackend,
  SyncIndexInvalid,
  // ---- commit ------------------------------------------------------------
  CommitReadFailed,
  CommitRestored,
  CommitBroken,
  CommitFailed,
  // ---- query -------------------------------------------------------------
  NotFound,
  NotFoundHintSources,
  NotFoundHintRepos,
  Provides,
  // ---- install -----------------------------------------------------------
  FileOne,
  FileMany,
  InstallBackendNoManager,
  InstallPackageNotFound,
  InstallPicaTooOld,
  InstallInvalidFilename,
  InstallDownloading,
  InstallDownloadingUrl,
  InstallSourceInvalid,
  InstallVerifying,
  InstallShaMismatch,
  InstallInvalidSha,
  InstallInvalidDownloadUrl,
  InstallRollingBack,
  InstallRolledBack,
  InstallRollbackLeftovers,
  InstallRollbackRecordFailed,
  InstallRollbackIncomplete,
  InstallReportFailed,
  TransactionCompleted,
  // ---- remove ------------------------------------------------------------
  RemoveHeader,
  RemoveHeaderVersioned,
  RemovePackage,
  RemoveSummary,
  RemoveSummaryOnly,
  RemoveSummaryOwned,
  RemoveBackendInferred,
  RemoveSkipScript,
  RemoveRunScript,
  // ---- upgrade -----------------------------------------------------------
  UpgradeNone,
  UpgradeSkip,
  UpgradeUpToDate,
  UpgradeAllUpToDate,
  InstallOrphanKept,
  InstallOrphansRemoved,
  QueryNoOwner,
  QueryOwnedBy,
  SearchEmptyQuery,
  SearchUnknownSource,
  SearchNotSynced,
  SearchInstalled,
  SearchNotInstallable,
  SearchHidden,
  IndexStale,
  NotFoundSuggest,
  UpgradeFailed,
  UpgradeSummary,
  UpgradeSomeFailed,
  ConfirmProceed,
  Cancelled,
  DryRunDone,
  DryRunNotApplicable,
  PlanPayload,
  PlanBackendInstall,
  PlanRemoveFiles,
  PlanRemoveBackend,
  PlanUpgrades,
  BatchTargets,
  BatchSummary,
  BatchSomeFailed,
  // ---- messages of the commands, backends and fetchers -----------------
  NoPackageManager,
  MissingCommand,
  InvalidPkgmgr,
  OptionalFlagsExclusive,
  InvalidFeedPolicy,
  InvalidPkgmgrFlag,
  InvalidLangFlag,
  MkdirFailed,
  InvalidApkForceSetting,
  ConstraintUnmet,
  ConstraintUnmetInFeed,
  DepPrecheckOtherBackend,
  LockBeingTaken,
  LockWithoutPid,
  UnknownArg,
  ArchFromUname,
  ArchUnknown,
  NotAbsoluteUrl,
  UrlWithCredentials,
  RemoteSourceLocalUrl,
  PlainHttpPublic,
  UnsupportedScheme,
  RedirectDowngrade,
  UnsafeRelativePath,
  SourceRootNotAbsolute,
  RedirectNoLocation,
  RedirectProtocolRelative,
  RedirectUnresolvable,
  NotPicaDatabase,
  DatabaseEncodeFailed,
  IndexMissing,
  JsonRootNotObject,
  JsonFieldNotObject,
  CacheCleanupSkipped,
  CacheCleanupFailed,
  InvalidFetchTimeout,
  ReadFileUrlFailed,
  NoFetchTool,
  TooManyRedirects,
  DownloadFailed,
  TempHeadersFailed,
  TempBodyFailed,
  CurlFailed,
  CurlDownloadFailed,
  ReadRedirectHeadersFailed,
  HttpNoLocation,
  HttpStatus,
  ReadBodyFailed,
  WgetFailed,
  WgetDownloadFailed,
  CommandExited,
  TarFailed,
  TarExtractFailed,
  FetcherDownloadFailed,
  HookNotFound,
  HookRunning,
  UnknownError,
  KeepingOwnedFile,
  IndexReposNotObject,
  SyncDepCheckNoManager,
  SyncDepCheckSkipped,
  SyncFeedMissingDep,
  SyncFeedMissingSome,
  ConstraintsUnreadable,
  BackupUnsupportedKind,
  BackupRemakeDirFailed,
  BackupStoreFailed,
  BackupRecreateFailed,
  ReadFailed,
  BackupReadFailed,
  BackupCopyFailed,
  OptionalIndexUnavailable,
  CheckingDeps,
  DepPrecheckMissing,
  DependenciesUninstalled,
  DepPrecheckFailed,
  InstallingOptional,
  DepIsPicaPackage,
  PicaArtifactWithPkgs,
  AppPhaseUnknownAdded,
  FeedOnlyNothingDeclared,
  PackagedOnlyNoContainers,
  FeedsPrompt,
  NoPackagesInDir,
  GuardSymlinkUnknown,
  GuardPathUnknown,
  GuardPicaState,
  ClearFailed,
  ScriptNotFound,
  CopyScriptFailed,
  OptionalSkipped,
  OptionalNoTerminal,
  OptionalSelectPrompt,
  OptionalTitle,
  OptionalFromPkgs,
  OptionalAlreadyInstalled,
  OptionalInstalledUnmetReq,
  OptionalInstalledUnmet,
  OptionalSizes,
  OptionalNotNumber,
  OptionalNotOffered,
  OptionalSizesUnknown,
  CopyEnvFailed,
  ManifestMissing,
  InvalidManifestPkgmgr,
  PkgmgrConflict,
  HeaderInstalling,
  HeaderAlreadyInstalled,
  HeaderUpgrading,
  HeaderDowngrading,
  HeaderReinstalling,
  InstalledSummary,
  PkgfileNotFound,
  PackageMissingManifest,
  InvalidManifest,
  ForeignContainer,
  DepsChangedVersion,
  OptionalDepSkipped,
  InstalledVersionUnknown,
  ArchFamilyMatch,
  ArchUnchecked,
  UnsupportedArch,
  UnsupportedUname,
  UnsupportedOs,
  PackageEmpty,
  InvalidSourceAvailability,
  LuciTypeNeedsVariant,
  InvalidLuci,
  LuciUndetectable,
  UnsupportedLuci,
  BackupCleanupFailed,
  InstalledListUnreadable,
  RestoreFailed,
  TheReplacedFiles,
  ApkNoCacheDir,
  NoIndexFile,
  NoCachedIndex,
  ApkSearchFailed,
  ApkSearchExit,
  ApkSearchKilled,
  ApkAddFailed,
  InstallStorageFull,
  BackendInstallFailed,
  ApkDelFailed,
  ApkDelTargetFailed,
  ApkListFailed,
  ApkListExit,
  ApkMixedRequest,
  InvalidLocalApk,
  ApkTargetInvalid,
  ReadDirFailed,
  ReposNoKeyword,
  ReposUnknownKeyword,
  ReposNoUrl,
  ReposInvalidUrl,
  ReposInvalidOption,
  ReposInvalidVariable,
  ReposNoArch,
  ReposUnterminated,
  ReposVariableLength,
  ReposUndefined,
  IndexReposFrom,
  IndexUnavailableDetail,
  KilledBySignal,
  UpdateFailedSilent,
  UpdateFailedStatus,
  NoRefreshCommand,
  UpdateFailed,
  PackagesAndMore,
  NotTextIndex,
  NoFeedIndex,
  UpdateBlocked,
  OpkgInfoFailed,
  OpkgInfoExit,
  OpkgInstallFailed,
  OpkgRemoveFailed,
  OpkgRemoveTargetFailed,
  OpkgListFailed,
  OpkgListExit,
  RequiresArgument,
  WarningLine,
  BoundUnmet,
  OriginRecorded,
  OriginLegacySource,
  OriginLegacyPkgfile,
  OriginDefault,
  GuardConfigFile,
  GuardDatabase,
  GuardIndex,
  GuardInstallReport,
  GuardLockFile,
  GuardLockDir,
  GuardKeptScripts,
  GuardEnvFragments,
  GuardStateDir,
  GuardConfigDir,
  ReasonFromPkgs,
  ReasonNoSource,
  ReasonPackagedButFeeds,
  ReasonFeedsUnreadable,
  ReasonNotInFeeds,
  ReasonPicaCannotInstall,
  OptionalUnavailable,
  SizeUnknown,
  SizeEstimated,
  OptionalSizesNote,
  SizeInstalledLabel,
  SizeDownloadLabel,
  ApkNeedsForce,
  ApkRejectsForce,
  Usage,
  // ---- messages of the commands, backends and fetchers -----------------
  InvalidOptionValue,
  ArchReportedBy,
  CommandFailed,
  IndexLabel,
  PackagesList,
  ExitCode,
  // ---- shared errors -----------------------------------------------------
  NotInstalled,
  LockBusy,
  LockCleanupFailed,
  LockUnverified,
  LockFailed,
  InvalidPkgname,
  InvalidAppname,
  TooManyArguments,
  UnexpectedArgument,
}

/// The English wording. Exhaustive: every key has a string here.
#[allow(
  clippy::too_many_lines,
  reason = "one arm per phrase; splitting the table would only hide where a phrase lives"
)]
fn english(key: Key) -> &'static str {
  match key {
    Key::SetupTitle => "Setup Pica",
    Key::SetupNeedsTerminal => {
      "pica setup needs an interactive terminal (stdin is not a tty). Write the configuration \
       directly and sync it: pica.json has \"language\" (zh-CN or en-US) and \"repos\" entries \
       with \"url\" and \"backends\"; then run `pica -S`."
    }
    Key::SetupConfUnreadable => {
      "Cannot read {0}: {1}. Fix or remove it, then run pica setup again."
    }
    Key::SetupNotAnObject => "the file is not a JSON object",
    Key::SetupCancelled => "Setup cancelled; no configuration was written.",
    Key::SetupInvalidChoice => "Invalid choice: pick a number from 1 to {0}.",
    Key::SetupLanguagePrompt => "Select language (Enter = English)",
    Key::SetupSources => "Configured sources",
    Key::SetupSourceLine => "  {0}. {1}  {2}  (backends: {3})",
    Key::SetupSourcePrompt => "Select a source (Enter = {0})",
    Key::SetupSourceOfficial => "Official source ({0})",
    Key::SetupSourceCustom => "Custom source URL",
    Key::SetupSourceKeep => "Keep the configured sources",
    Key::SetupAllowHttp => "This source is plain HTTP. Allow it for {0}?",
    Key::SetupYes => "Yes",
    Key::SetupNo => "No",
    Key::SetupSourceUrl => "Source root URL: ",
    Key::SetupUrlEmpty => "A source root URL is required.",
    Key::SetupOfficialMissing => {
      "The official source address is not configured in this build. Set OFFICIAL_REPO_ROOT in \
       crates/pica-pkg-cli/src/commands/setup.rs, or pass a source root URL."
    }
    Key::SetupNoCommonBackend => {
      "{0}: no backend in common - this host supports {1}, the source declares {2}; nothing \
       was written"
    }
    Key::SetupBackends => "{0}: using backend(s) {1}",
    Key::SetupSaved => "Configuration saved to {0}.",
    Key::SetupComplete => "Setup complete: language {0}; sources: {1}.",
    Key::SetupNextSteps => {
      "Next: `pica -Si <package>` shows a package, `pica -S <package>` installs it, `pica -S` \
       re-syncs the sources."
    }
    Key::SetupNotJson => {
      "pica setup is interactive and does not support --json or --json-errors; run it without them"
    }

    Key::SyncStarted => "Synchronizing package databases...",
    Key::SyncDownloading => "{0} downloading...",
    Key::SyncUpdated => "{0} updated",
    Key::SyncPackageSkipped => "{0}: skipped an entry of the {1} index: invalid {2} {3}",
    Key::SyncNoRepos => {
      "no repos configured in {0}; add at least one entry with \"url\" and \"backends\""
    }
    Key::SyncReposNotArray => "config {0} is not an array",
    Key::SyncRepoMissingName => "repo[{0}] missing name",
    Key::SyncRepoMissingUrl => "repo[{0}] missing url",
    Key::SyncBackendsMissing => "repo[{0}] ({1}) has no backends; run `pica setup` to choose them",
    Key::SyncBackendsNotArray => "repo[{0}] ({1}) backends must be an array of backend names",
    Key::SyncUnsupportedBackend => {
      "repo[{0}] ({1}) backends contains unsupported backend {2} (supported: {3})"
    }
    Key::SyncBackendsEmpty => "repo[{0}] ({1}) backends is empty; name at least one backend",
    Key::SyncRootRejected => "{0}: source root {1}: {2}",
    Key::SyncFetchFailed => "{0}: fetching {1} from {2} failed: {3}",
    Key::SyncNotUtf8 => "{0}: {1} at {2} is not UTF-8",
    Key::SyncEntryInvalid => "{0}: index.json at {1} is invalid: {2}",
    Key::SyncMissingBackend => {
      "{0}: source does not provide backend(s) {1} (declares: {2}); adjust its backends list or \
       use a source that does"
    }
    Key::SyncIndexInvalid => "{0}: {1} index at {2} is invalid: {3}",

    Key::CommitReadFailed => "cannot read {0}: {1}",
    Key::CommitRestored => "could not write {0}; the previous state was put back",
    Key::CommitBroken => "could not write {0}, and restoring the previous state failed too: {1}",
    Key::CommitFailed => "could not write {0}: {1}",

    Key::NotFound => "package not found in pica repos: {0}{1}",
    Key::NotFoundHintSources => " (backend {0}; no synced sources, run -S first)",
    Key::NotFoundHintRepos => " (backend {0}; {1})",
    Key::Provides => "{0} provides {1}",

    Key::InstallBackendNoManager => {
      "{0}: backend has no package manager, use selectors from pica repos instead"
    }
    Key::InstallPackageNotFound => "{0}: package not found: {1}",
    Key::InstallPicaTooOld => "pica too old: pkg requires >= {0}, cli is {1}",
    Key::InstallInvalidFilename => "{0}: invalid filename: {1}",
    Key::InstallDownloading => "Downloading {0} ({1}) from {2}...",
    Key::InstallDownloadingUrl => "Downloading pkg from URL: {0}",
    Key::InstallSourceInvalid => "pkg source not found or unsupported URL: {0}",
    Key::FileOne => "1 file",
    Key::FileMany => "{0} files",
    Key::InstallVerifying => "Verifying SHA-256...",
    Key::InstallShaMismatch => "sha256 mismatch for {0}: expected {1}, got {2}",
    Key::InstallInvalidSha => "{0}: invalid sha256 for {1}",
    Key::InstallInvalidDownloadUrl => "{0}: invalid download_url for {1}: {2}",
    Key::InstallRollingBack => "{0}: install failed, rolling back",
    Key::InstallRolledBack => "{0}: rolled back to the previous state",
    Key::InstallRollbackLeftovers => "{0}: rollback left {1} path(s) behind: {2}",
    Key::InstallRollbackRecordFailed => "{0}: could not record the partial install: {1}",
    Key::InstallRollbackIncomplete => "{0}; rollback incomplete, see the warnings above",
    Key::InstallReportFailed => "{0}: installed, but the install report could not be written: {1}",
    Key::TransactionCompleted => "Transaction completed",

    Key::RemoveHeader => "Removing {0}",
    Key::RemoveHeaderVersioned => "Removing {0} ({1})",
    Key::RemovePackage => "Removing {0} package: {1}",
    Key::RemoveSummary => "Removed {0}; kept {1}",
    Key::RemoveSummaryOnly => "Removed {0}",
    Key::RemoveSummaryOwned => "{0} owned by {1}",
    Key::RemoveBackendInferred => "{0}: backend {1}; using {2}",
    Key::RemoveSkipScript => "Skip {0} script: {1} (not found at {2})",
    Key::RemoveRunScript => "Running {0} script: {1}",

    Key::UpgradeNone => "No pica packages to upgrade",
    Key::UpgradeSkip => "Skip {0}: not found in index",
    Key::UpgradeUpToDate => "Up to date: {0} ({1})",
    Key::UpgradeAllUpToDate => "All packages are up to date",
    Key::InstallOrphanKept => "{0}: could not remove {1}, which the new version no longer ships",
    Key::InstallOrphansRemoved => "Removed {0} file(s) the previous version left",
    Key::QueryNoOwner => "no installed package owns {0}",
    Key::QueryOwnedBy => "{0} is owned by {1} ({2})",
    Key::SearchEmptyQuery => "a search needs at least one term",
    Key::SearchUnknownSource => "no synced source is named {0}",
    Key::SearchNotSynced => "no source has been synced yet: run pica -S first",
    Key::SearchInstalled => "[installed: {0}]",
    Key::SearchNotInstallable => "(not installable here: {0})",
    Key::SearchHidden => "{0} result(s) not installable here are hidden; add --all to show them",
    Key::IndexStale => "the synced index is {0} day(s) old; run pica -S to refresh it",
    Key::NotFoundSuggest => "; did you mean: {0}",
    Key::UpgradeFailed => "{0}: upgrade failed: {1}",
    Key::UpgradeSummary => "Upgrade finished: {0} upgraded, {1} skipped, {2} failed",
    Key::UpgradeSomeFailed => "some packages failed to upgrade: {0}",
    Key::ConfirmProceed => "Proceed?",
    Key::Cancelled => "cancelled; nothing was changed",
    Key::DryRunDone => "Dry run: nothing was changed",
    Key::DryRunNotApplicable => "--dry-run does not apply to {0}",
    Key::PlanPayload => "  Payload: {0}, {1}",
    Key::PlanBackendInstall => "Packages to install via {0}: {1}",
    Key::PlanRemoveFiles => "  Files to remove: {0}",
    Key::PlanRemoveBackend => "  Packages to remove via {0}: {1}",
    Key::PlanUpgrades => "Packages to upgrade ({0}):",
    Key::BatchTargets => "Targets ({0}): {1}",
    Key::BatchSummary => "Finished: {0} succeeded, {1} failed",
    Key::BatchSomeFailed => "some targets failed: {0}",

    Key::NotInstalled => "not installed: {0}",
    Key::LockBusy => "cannot lock pica database: {0} exists (held by pid {1})",
    Key::LockUnverified => {
      "cannot lock pica database: {0} exists and this host cannot check whether {1} still holds it, so the lock was left in place; remove the directory if no other pica is running"
    }
    Key::LockCleanupFailed => "cannot lock pica database: stale lock cleanup failed for {0}: {1}",
    Key::LockFailed => "cannot lock pica database: {0}",
    Key::InvalidPkgname => {
      "invalid pkgname {0}: start with a letter or digit and use only letters, digits and . _ + - @"
    }
    Key::InvalidAppname => {
      "invalid appname {0}: start with a letter or digit and use only letters, digits and . _ + -"
    }
    Key::TooManyArguments => "{0} takes one argument; unexpected argument: {1}",
    Key::UnexpectedArgument => "{0} takes no argument; unexpected argument: {1}",
    Key::NoPackageManager => "no supported package manager found",
    Key::MissingCommand => "missing required command: {0}",
    Key::InvalidPkgmgr => "invalid pkgmgr: {0}",
    Key::OptionalFlagsExclusive => "--with-optional and --no-optional are mutually exclusive",
    Key::InvalidFeedPolicy => "invalid --feed-policy: {0}",
    Key::InvalidPkgmgrFlag => "invalid --pkgmgr: {0}",
    Key::InvalidLangFlag => "invalid --lang: {0}",
    Key::MkdirFailed => "mkdir {0} failed: {1}",
    Key::InvalidApkForceSetting => {
      "invalid \"apk_force_non_repository\" in {0}: expected true or false, found {1}"
    }
    Key::ConstraintUnmet => "{0} requires {1}:{2}, but {3} is installed",
    Key::ConstraintUnmetInFeed => {
      "{0} requires {1}:{2}, but the feeds offer only {3}"
    }
    Key::DepPrecheckOtherBackend => {
      "; {0} is installed on this host but was not searched, only {1} was — \
       retry with --pkgmgr {0}"
    }
    Key::LockBeingTaken => "a lock being taken",
    Key::LockWithoutPid => "a lock without a pid",
    Key::UnknownArg => "unknown arg: {0}",
    Key::ArchFromUname => "no package manager reported an arch list; uname -m reports {0}",
    Key::ArchUnknown => "no package manager reported an arch list and uname -m could not be read",
    Key::NotAbsoluteUrl => "not an absolute URL: {0}",
    Key::UrlWithCredentials => "refusing a URL with credentials in it: {0}",
    Key::RemoteSourceLocalUrl => "a remote source may not download from this machine: {0}",
    Key::PlainHttpPublic => {
      "plain HTTP is only used for local sources; {0} is public — use https, or set allow_insecure_http for this source if that is really what you want"
    }
    Key::UnsupportedScheme => "unsupported URL scheme {0}: {1}",
    Key::RedirectDowngrade => "refusing to follow a redirect from https to {0}",
    Key::UnsafeRelativePath => "{0} is not a safe relative path: {1}",
    Key::SourceRootNotAbsolute => "source root is not an absolute URL: {0}",
    Key::RedirectNoLocation => "redirect without a Location value",
    Key::RedirectProtocolRelative => "refusing a protocol-relative redirect: {0}",
    Key::RedirectUnresolvable => "cannot resolve a redirect from {0}",
    Key::NotPicaDatabase => "{0} is not a pica database: {1}",
    Key::DatabaseEncodeFailed => "cannot encode the database: {0}",
    Key::IndexMissing => "missing index: run 'pica -S' first ({0})",
    Key::JsonRootNotObject => "json root is not object",
    Key::JsonFieldNotObject => "json field '{0}' is not object",
    Key::CacheCleanupSkipped => "cleanup cache skipped: cannot read {0}: {1}",
    Key::CacheCleanupFailed => "cleanup cache failed: {0}: {1}",
    Key::InvalidFetchTimeout => "invalid --fetch-timeout: {0}",
    Key::ReadFileUrlFailed => "read file url failed: {0}",
    Key::NoFetchTool => "no fetch tool found (need uclient-fetch, wget, or curl)",
    Key::TooManyRedirects => "too many redirects (>{0}) starting at {1}",
    Key::DownloadFailed => "download failed after {0} attempts (timeout={1}s): {2} detail=[{3}]",
    Key::TempHeadersFailed => "temp file for headers failed: {0}",
    Key::TempBodyFailed => "temp file for body failed: {0}",
    Key::CurlFailed => "curl failed: {0}",
    Key::CurlDownloadFailed => "curl download failed: {0}",
    Key::ReadRedirectHeadersFailed => "read redirect headers failed: {0}",
    Key::HttpNoLocation => "HTTP {0} without a Location header: {1}",
    Key::HttpStatus => "HTTP {0} for {1}",
    Key::ReadBodyFailed => "read download body failed: {0}",
    Key::WgetFailed => "wget failed: {0}",
    Key::WgetDownloadFailed => "wget download failed: {0}",
    Key::CommandExited => "{0} exited with status {1}",
    Key::TarFailed => "tar extract failed: {0}",
    Key::TarExtractFailed => "tar extract failed: {0}: {1}",
    Key::FetcherDownloadFailed => "{0} download failed: {1}",
    Key::HookNotFound => "{0} hook not found: {1}",
    Key::HookRunning => "Running {0} hook: {1}",
    Key::UnknownError => "unknown error",
    Key::KeepingOwnedFile => "keeping {0}: still owned by {1}",
    Key::IndexReposNotObject => "index repos is not object",
    Key::SyncDepCheckNoManager => "skip dependency check: no supported package manager found",
    Key::SyncDepCheckSkipped => "skip dependency check: {0}",
    Key::SyncFeedMissingDep => "{0} feed missing dependency: {1}",
    Key::SyncFeedMissingSome => "Some declared dependencies are missing in {0} feeds",
    Key::ConstraintsUnreadable => "{0}: cannot read {1} constraints: {2}",
    Key::BackupUnsupportedKind => {
      "{0} is neither a file, a symlink nor a directory; this install stops before writing anything, because pica could not put that back"
    }
    Key::BackupRemakeDirFailed => "could not make {0} a directory again: {1}",
    Key::BackupStoreFailed => {
      "could not make a directory for the files this install replaces ({0}): {1}"
    }
    Key::BackupRecreateFailed => "could not recreate {0}: {1}",
    Key::ReadFailed => "read {0} failed: {1}",
    Key::BackupReadFailed => {
      "could not read {0} before replacing it ({1}); nothing has been written"
    }
    Key::BackupCopyFailed => "could not keep a copy of {0} at {1} ({2}); nothing has been written",
    Key::OptionalIndexUnavailable => {
      "dependency index unavailable, optional dependencies cannot be offered: {0}"
    }
    Key::CheckingDeps => "Checking declared dependencies...",
    Key::DepPrecheckMissing => "dependency precheck: missing {0}",
    Key::DependenciesUninstalled => {
      "no opkg or apk on this host, so these dependencies were not installed: {0}"
    }
    Key::DepPrecheckFailed => "dependency precheck failed, missing: {0}{1}",
    Key::InstallingOptional => "Installing {0} optional dependencies on request",
    Key::DepIsPicaPackage => {
      "; {0} exist in the Pica sources, but pica does not resolve Pica packages as dependencies — declare a system package name instead, or ship the container in this artifact's pkgs/"
    }
    Key::PicaArtifactWithPkgs => {
      "a pica-backend artifact must not carry {0}/: a pica artifact ships {1}/ and scripts/, and pica does not hand containers to opkg or apk. Rebuild it with `targets = opkg` or `targets = apk` so the device's package manager installs them, or extract them into {2}/ before packing"
    }
    Key::AppPhaseUnknownAdded => "could not tell which packages the app phase added: {0}",
    Key::FeedOnlyNothingDeclared => {
      "feed-only policy: the artifact declares no packages, and its packaged containers may not be used (declare them, or install with another --feed-policy)"
    }
    Key::PackagedOnlyNoContainers => {
      "packaged-only policy: the artifact ships no containers, and feeds may not be used"
    }
    Key::FeedsPrompt => {
      "Found {0}/{1} declared packages in the {2} feeds. Use feeds instead of the packaged containers?"
    }
    Key::NoPackagesInDir => "no packages found in {0} dir: {1}",
    Key::GuardSymlinkUnknown => "cannot tell where the symlink {0} would lead: {1}",
    Key::GuardPathUnknown => "cannot tell where payload path {0} would land: {1}",
    Key::GuardPicaState => "payload writes Pica's own {0}: {1}",
    Key::ClearFailed => "cannot clear {0}: {1}",
    Key::ScriptNotFound => "script not found in package: {0}",
    Key::CopyScriptFailed => "copy script failed: {0}",
    Key::OptionalSkipped => "optional dependencies skipped ({0}): {1}",
    Key::OptionalNoTerminal => {
      "pica: stdin is not a terminal; installing no optional dependencies (use --with-optional to include them)"
    }
    Key::OptionalSelectPrompt => {
      "Select optional packages by number, `all`, or press Enter to skip: "
    }
    Key::OptionalTitle => "Optional dependencies for {0}\n\n",
    Key::OptionalFromPkgs => "installed from the artifact's pkgs/ with this request",
    Key::OptionalAlreadyInstalled => "already installed",
    Key::OptionalInstalledUnmetReq => "already installed, but {0} ({1})",
    Key::OptionalInstalledUnmet => "already installed, but {0}",
    Key::OptionalSizes => "Download: {0}  Installed: {1}",
    Key::OptionalNotNumber => "`{0}` is not a number; nothing was selected",
    Key::OptionalNotOffered => "`{0}` is not one of the offered numbers; nothing was selected",
    Key::OptionalSizesUnknown => "optional dependency sizes are unknown: {0}",
    Key::CopyEnvFailed => "copy env file failed: {0}",
    Key::ManifestMissing => "manifest missing {0}",
    Key::InvalidManifestPkgmgr => "invalid manifest pkgmgr: {0}",
    Key::PkgmgrConflict => "--pkgmgr {0} conflicts with package pkgmgr {1}",
    Key::HeaderInstalling => "Installing {0} ({1})",
    Key::HeaderAlreadyInstalled => "{0} {1} is already installed; reinstalling",
    Key::HeaderUpgrading => "Upgrading {0} ({1} -> {2})",
    Key::HeaderDowngrading => "Downgrading {0} ({1} -> {2})",
    Key::HeaderReinstalling => "Reinstalling {0} ({1})",
    Key::InstalledSummary => "Installed {0}, {1}",
    Key::PkgfileNotFound => "pkgfile not found: {0}",
    Key::PackageMissingManifest => "package missing manifest",
    Key::InvalidManifest => "invalid manifest: {0}",
    Key::ForeignContainer => {
      "{0} is an {1} package, not a Pica artifact: install it with `{2} {3}`, or ship it inside a Pica package under pkgs/{4}/"
    }
    Key::DepsChangedVersion => {
      "dependencies changed version during this install and a rollback would not restore them: {0}"
    }
    Key::OptionalDepSkipped => "optional dependency {0} was skipped: {1}",
    Key::InstalledVersionUnknown => {
      "installed version of {0} is unknown, so the declared {1} was not checked"
    }
    Key::ArchFamilyMatch => "arch {0} was accepted on a family match alone ({1})",
    Key::ArchUnchecked => {
      "cannot determine this host's package architecture, so arch={0} was not checked ({1})"
    }
    Key::UnsupportedArch => "unsupported arch: pkg={0} ({1})",
    Key::UnsupportedUname => "unsupported uname: pkg={0} host={1}",
    Key::UnsupportedOs => "unsupported os: pkg={0} host={1}",
    Key::PackageEmpty => {
      "package would be empty: it ships no root/ or scripts/, and its pkgs/ holds no packages"
    }
    Key::InvalidSourceAvailability => "invalid source_availability value: {0} (supported: {1})",
    Key::LuciTypeNeedsVariant => "type=luci requires luci=<lua1|js2>",
    Key::InvalidLuci => "invalid luci value: {0}",
    Key::LuciUndetectable => "luci variant required ({0}) but cannot detect host",
    Key::UnsupportedLuci => "unsupported luci variant: pkg={0} host={1}",
    Key::BackupCleanupFailed => {
      "the install completed, but the copies taken of the files it replaced could not be removed from {0} ({1})"
    }
    Key::InstalledListUnreadable => {
      "could not read the installed package list; the rollback record may be incomplete: {0}"
    }
    Key::RestoreFailed => {
      "pica could not put {0} back; the copies it kept are in {1}, which is the only place those files still exist"
    }
    Key::TheReplacedFiles => "the files it replaced",
    Key::ApkNoCacheDir => "no index cache directory ({0}) for {1} repositories",
    Key::NoIndexFile => "no index file at {0}",
    Key::NoCachedIndex => "no cached index for {0}",
    Key::ApkSearchFailed => "apk search --exact {0} failed: {1}",
    Key::ApkSearchExit => "apk search --exact {0} failed (exit {1}): {2}",
    Key::ApkSearchKilled => "apk search --exact {0} was killed by a signal: {1}",
    Key::ApkAddFailed => "apk add failed: {0}",
    Key::InstallStorageFull => "{0} install failed: {1} (storage-full). detail=[{2}]",
    Key::BackendInstallFailed => "{0} install failed: {1} detail=[{2}]",
    Key::ApkDelFailed => "apk del failed: {0}",
    Key::ApkDelTargetFailed => "apk del failed: {0} detail=[{1}]",
    Key::ApkListFailed => "apk list --installed failed: {0}",
    Key::ApkListExit => "apk list --installed failed ({0}): {1}",
    Key::ApkMixedRequest => "one apk request cannot mix local apk files with package names",
    Key::InvalidLocalApk => "invalid local apk: {0}",
    Key::ApkTargetInvalid => "expected a package name or an existing local apk file",
    Key::ReadDirFailed => "read directory {0} failed: {1}",
    Key::ReposNoKeyword => "no keyword or URL",
    Key::ReposUnknownKeyword => "unrecognized keyword: {0}",
    Key::ReposNoUrl => "no URL",
    Key::ReposInvalidUrl => "invalid URL: {0}",
    Key::ReposInvalidOption => "invalid option: {0}",
    Key::ReposInvalidVariable => "invalid variable definition: {0}",
    Key::ReposNoArch => "cannot tell which index {0} needs: {1} does not name a machine type",
    Key::ReposUnterminated => "unterminated variable: {0}",
    Key::ReposVariableLength => "invalid variable length: {0}",
    Key::ReposUndefined => "undefined variable: {0}",
    Key::IndexReposFrom => " (repositories come from {0})",
    Key::IndexUnavailableDetail => "{0} index unavailable: {1}{2}{3}",
    Key::KilledBySignal => "killed by a signal",
    Key::UpdateFailedSilent => "{0} update failed ({1}, no output)",
    Key::UpdateFailedStatus => "{0} update failed ({1}): {2}",
    Key::NoRefreshCommand => "no index refresh command is configured",
    Key::UpdateFailed => "{0} update failed: {1}",
    Key::PackagesAndMore => "{0} packages ({1}, and {2} more)",
    Key::NotTextIndex => "{0} is not a text index, so its entries cannot be read",
    Key::NoFeedIndex => "no index for feed {0} ({1})",
    Key::UpdateBlocked => "{0} update is blocked: {1} is held by pid {2}",
    Key::OpkgInfoFailed => "opkg info {0} failed: {1}",
    Key::OpkgInfoExit => "opkg info {0} failed ({1}): {2}",
    Key::OpkgInstallFailed => "opkg install failed: {0}",
    Key::OpkgRemoveFailed => "opkg remove failed: {0}",
    Key::OpkgRemoveTargetFailed => "opkg remove failed: {0} detail=[{1}]",
    Key::OpkgListFailed => "opkg list-installed failed: {0}",
    Key::OpkgListExit => "opkg list-installed failed ({0}): {1}",
    Key::RequiresArgument => "{0} requires {1}",
    Key::WarningLine => "warning: {0}",
    Key::BoundUnmet => "the installed version does not satisfy the declared constraint",
    Key::OriginRecorded => "recorded in pkgmgr",
    Key::OriginLegacySource => "inferred from the legacy source field",
    Key::OriginLegacyPkgfile => "inferred from the legacy pkgfile field",
    Key::OriginDefault => "no backend recorded",
    Key::GuardConfigFile => "config file",
    Key::GuardDatabase => "database",
    Key::GuardIndex => "index",
    Key::GuardInstallReport => "install report",
    Key::GuardLockFile => "lock file",
    Key::GuardLockDir => "lock directory",
    Key::GuardKeptScripts => "kept scripts",
    Key::GuardEnvFragments => "env fragments",
    Key::GuardStateDir => "state directory",
    Key::GuardConfigDir => "configuration directory",
    Key::ReasonFromPkgs => "this artifact installs its dependencies from pkgs/",
    Key::ReasonNoSource => "this install has no dependency source",
    Key::ReasonPackagedButFeeds => "packaged in the artifact, but this install uses the feeds",
    Key::ReasonFeedsUnreadable => "the feeds could not be read",
    Key::ReasonNotInFeeds => "not available from the feeds",
    Key::ReasonPicaCannotInstall => "pkgmgr=pica cannot install dependencies",
    Key::OptionalUnavailable => "unavailable ({0})",
    Key::SizeUnknown => "Unknown",
    Key::SizeEstimated => "~{0} (est.)",
    Key::OptionalSizesNote => {
      "\nSizes are this package alone, as its feed records them; they exclude dependencies.\n"
    }
    Key::SizeInstalledLabel => "Package installed size",
    Key::SizeDownloadLabel => "Download size",
    Key::ApkNeedsForce => {
      "; a local apk needs --force-non-repository on apk 3.x for a package no repository provides, and this request ran without it (set \"apk_force_non_repository\": true in pica.json)"
    }
    Key::ApkRejectsForce => {
      "; this apk does not accept --force-non-repository (set \"apk_force_non_repository\": false in pica.json)"
    }
    Key::Usage => "Usage:\n  pica setup              Interactive setup: language, sources, backends, sync\n  pica -S [selector...]   Sync repos (no selector) or install by selector (auto)\n  pica -Su                Upgrade all installed pica packages\n  pica -Syu               Sync, then upgrade all installed pica packages\n  pica -Si <selector>     Show remote package info from synced index\n  pica -Ss <terms>        Search the synced sources (--all: include packages not installable here)\n  pica -Sl [source]       List what the synced sources publish\n  pica -Qs <terms>        Search installed packages\n  pica -Qo <path>         Show which package owns a file\n  pica -Qu                List installed packages with a newer version available\n  pica -So <selector>     Install via the selected system package manager\n  pica -Sp <selector...>  Install by selector (force pica repo)\n  pica -U <pkgfile|url>...\n                          Install/Update from local file or URL\n  pica -R <pkgname...>    Remove package (no dependency handling)\n  pica -Q                 List installed pica packages\n  pica -Qi <pkgname>      Show installed package info\n  pica -Ql <pkgname>      List installed package files\n  pica --json ...         Emit JSON on success and error (explicit only)\n  pica --json-errors ...  Emit JSON only on error\n  pica --non-interactive ...\n                            Disable prompts (for backend/automation)\n  pica --dry-run ...      Show the plan of -S/-Sp/-So/-U/-R/-Su and change nothing\n  pica --noconfirm ...    Do not ask before changing the system (alias --yes)\n  pica --feed-policy <mode>\n                            ask|feed-first|packaged-first|feed-only|packaged-only\n  pica --with-optional    Install every available optional dependency\n  pica --no-optional      Install no optional dependency (default off a terminal)\n  pica --pkgmgr <backend> opkg|apk|pica (overrides manifest and config)
  pica --lang <code>      Interface language for this run: zh-CN|en-US\n  pica -V\n  pica --version\n\nNotes:\n  - Requires: tar and one fetcher (uclient-fetch/wget/curl) for URL install/sync.\n  - Backends: apk, opkg, pica. Resolution order is --pkgmgr, then the package\n    manifest, then /etc/pica/pica.json \"pkgmgr\", then auto-detection (apk before opkg, else pica).\n  - Config: /etc/pica/pica.json\n  - State:  /var/lib/pica/db.json, /var/lib/pica/index.json\n  - Lock:   /var/lib/pica/db.lck\n  - Selector example: app(branch)",
    Key::InvalidOptionValue => "invalid {0}: {1}",
    Key::ArchReportedBy => "{0} reports {1}",
    Key::CommandFailed => "{0} failed: {1}",
    Key::IndexLabel => "{0} index",
    Key::PackagesList => "{0} packages ({1})",
    Key::ExitCode => "exit {0}",
  }
}

/// The Chinese wording. Exhaustive, like the English table: a key without a
/// translation is a compile error, not a mixed line.
#[allow(
  clippy::too_many_lines,
  reason = "one arm per phrase; splitting the table would only hide where a phrase lives"
)]
fn chinese(key: Key) -> &'static str {
  match key {
    Key::SetupTitle => "Pica 初始化",
    Key::SetupNeedsTerminal => {
      "pica setup 需要交互式终端（stdin 不是 tty）。请直接写配置文件后同步：pica.json 中的 \
       \"language\"（zh-CN 或 en-US）与 \"repos\" 条目（\"url\"、\"backends\"），再运行 `pica -S`。"
    }
    Key::SetupConfUnreadable => "无法读取 {0}：{1}。请修复或移走它后重新运行 pica setup。",
    Key::SetupNotAnObject => "文件不是一个 JSON 对象",
    Key::SetupCancelled => "已取消初始化，未写入任何配置。",
    Key::SetupInvalidChoice => "无效选择：请输入 1 到 {0} 之间的数字。",
    Key::SetupLanguagePrompt => "选择语言（回车 = English）",
    Key::SetupSources => "已配置的软件源",
    Key::SetupSourceLine => "  {0}. {1}  {2}  （后端：{3}）",
    Key::SetupSourcePrompt => "选择软件源（回车 = {0}）",
    Key::SetupSourceOfficial => "官方源（{0}）",
    Key::SetupSourceCustom => "自定义软件源地址",
    Key::SetupSourceKeep => "保留已配置的软件源",
    Key::SetupAllowHttp => "该软件源使用明文 HTTP，是否允许 {0}？",
    Key::SetupYes => "是",
    Key::SetupNo => "否",
    Key::SetupSourceUrl => "软件源根地址：",
    Key::SetupUrlEmpty => "必须填写软件源根地址。",
    Key::SetupOfficialMissing => {
      "此构建没有配置官方源地址。请在 crates/pica-pkg-cli/src/commands/setup.rs 的 \
       OFFICIAL_REPO_ROOT 中填写，或直接输入软件源根地址。"
    }
    Key::SetupNoCommonBackend => {
      "{0}：本机没有可用共同后端——本机支持 {1}，该源声明 {2}；未写入任何配置"
    }
    Key::SetupBackends => "{0}：使用后端 {1}",
    Key::SetupSaved => "配置已保存到 {0}。",
    Key::SetupComplete => "初始化完成：语言 {0}；软件源：{1}。",
    Key::SetupNextSteps => {
      "接下来：`pica -Si <包名>` 查看软件包，`pica -S <包名>` 安装，`pica -S` 重新同步软件源。"
    }
    Key::SetupNotJson => "pica setup 是交互式命令，不支持 --json / --json-errors，请去掉后重试。",

    Key::SyncStarted => "正在同步软件源……",
    Key::SyncDownloading => "正在下载 {0}...",
    Key::SyncUpdated => "{0} 已更新",
    Key::SyncPackageSkipped => "{0}：已跳过 {1} 索引中的一个条目：{2} 无效：{3}",
    Key::SyncNoRepos => "未在 {0} 中配置软件源；请至少添加一个含 \"url\" 与 \"backends\" 的条目",
    Key::SyncReposNotArray => "配置项 {0} 不是数组",
    Key::SyncRepoMissingName => "repo[{0}] 缺少 name",
    Key::SyncRepoMissingUrl => "repo[{0}] 缺少 url",
    Key::SyncBackendsMissing => "repo[{0}]（{1}）没有 backends；请运行 `pica setup` 选择",
    Key::SyncBackendsNotArray => "repo[{0}]（{1}）的 backends 必须是后端名数组",
    Key::SyncUnsupportedBackend => "repo[{0}]（{1}）的 backends 含不支持的后端 {2}（支持：{3}）",
    Key::SyncBackendsEmpty => "repo[{0}]（{1}）的 backends 为空；至少写一个后端",
    Key::SyncRootRejected => "{0}：软件源根地址 {1}：{2}",
    Key::SyncFetchFailed => "{0}：从 {2} 获取 {1} 失败：{3}",
    Key::SyncNotUtf8 => "{0}：{2} 的 {1} 不是 UTF-8",
    Key::SyncEntryInvalid => "{0}：{1} 的 index.json 无效：{2}",
    Key::SyncMissingBackend => {
      "{0}：软件源未提供后端 {1}（已声明：{2}）；请调整 backends 或改用提供该后端的源"
    }
    Key::SyncIndexInvalid => "{0}：{2} 的 {1} 索引无效：{3}",

    Key::CommitReadFailed => "无法读取 {0}：{1}",
    Key::CommitRestored => "写入 {0} 失败；已恢复原有状态",
    Key::CommitBroken => "写入 {0} 失败，且恢复原有状态同样失败：{1}",
    Key::CommitFailed => "无法写入 {0}：{1}",

    Key::NotFound => "pica 软件源中未找到软件包：{0}{1}",
    Key::NotFoundHintSources => "（后端 {0}；尚未同步软件源，请先运行 -S）",
    Key::NotFoundHintRepos => "（后端 {0}；{1}）",
    Key::Provides => "{0} 提供 {1}",

    Key::InstallBackendNoManager => "{0}：该后端没有包管理器，请改用 pica 软件源中的选择器",
    Key::InstallPackageNotFound => "{0}：未找到软件包：{1}",
    Key::InstallPicaTooOld => "pica 版本过旧：软件包要求 >= {0}，当前 CLI 为 {1}",
    Key::InstallInvalidFilename => "{0}：文件名无效：{1}",
    Key::InstallDownloading => "正在从 {2} 下载 {0}（{1}）...",
    Key::InstallDownloadingUrl => "正在从 URL 下载软件包：{0}",
    Key::InstallSourceInvalid => "软件包来源不存在或 URL 不受支持：{0}",
    Key::FileOne => "1 个文件",
    Key::FileMany => "{0} 个文件",
    Key::InstallVerifying => "正在校验 SHA-256...",
    Key::InstallShaMismatch => "{0} 的 sha256 不匹配：期望 {1}，实际 {2}",
    Key::InstallInvalidSha => "{0}：{1} 的 sha256 无效",
    Key::InstallInvalidDownloadUrl => "{0}：{1} 的 download_url 无效：{2}",
    Key::InstallRollingBack => "{0}：安装失败，正在回滚",
    Key::InstallRolledBack => "{0}：已回滚到之前的状态",
    Key::InstallRollbackLeftovers => "{0}：回滚后仍残留 {1} 个路径：{2}",
    Key::InstallRollbackRecordFailed => "{0}：无法记录这次部分安装：{1}",
    Key::InstallRollbackIncomplete => "{0}；回滚未完成，请查看上面的警告",
    Key::InstallReportFailed => "{0}：已安装，但安装报告写入失败：{1}",
    Key::TransactionCompleted => "事务完成",

    Key::RemoveHeader => "正在卸载 {0}",
    Key::RemoveHeaderVersioned => "正在卸载 {0}（{1}）",
    Key::RemovePackage => "正在卸载 {0} 软件包：{1}",
    Key::RemoveSummary => "已删除 {0}；保留 {1}",
    Key::RemoveSummaryOnly => "已删除 {0}",
    Key::RemoveSummaryOwned => "{0}（归 {1} 所有）",
    Key::RemoveBackendInferred => "{0}：后端 {1}；使用 {2}",
    Key::RemoveSkipScript => "跳过 {0} 脚本：{1}（未在 {2} 找到）",
    Key::RemoveRunScript => "正在运行 {0} 脚本：{1}",

    Key::UpgradeNone => "没有可升级的 pica 软件包",
    Key::UpgradeSkip => "跳过 {0}：索引中未找到",
    Key::UpgradeUpToDate => "已是最新：{0}（{1}）",
    Key::UpgradeAllUpToDate => "所有软件包均为最新",
    Key::InstallOrphanKept => "{0}：无法删除新版本不再携带的 {1}",
    Key::InstallOrphansRemoved => "已删除上一版本遗留的 {0} 个文件",
    Key::QueryNoOwner => "没有已安装的软件包拥有 {0}",
    Key::QueryOwnedBy => "{0} 属于 {1}（{2}）",
    Key::SearchEmptyQuery => "搜索至少需要一个关键词",
    Key::SearchUnknownSource => "没有名为 {0} 的已同步软件源",
    Key::SearchNotSynced => "尚未同步任何软件源：请先运行 pica -S",
    Key::SearchInstalled => "[已安装：{0}]",
    Key::SearchNotInstallable => "（本机不可安装：{0}）",
    Key::SearchHidden => "有 {0} 个本机不可安装的结果已隐藏；加 --all 显示",
    Key::IndexStale => "已同步的索引是 {0} 天前的；运行 pica -S 更新",
    Key::NotFoundSuggest => "；你是否要找：{0}",
    Key::UpgradeFailed => "{0}：升级失败：{1}",
    Key::UpgradeSummary => "升级完成：{0} 个已升级，{1} 个已跳过，{2} 个失败",
    Key::UpgradeSomeFailed => "部分软件包升级失败：{0}",
    Key::ConfirmProceed => "是否继续？",
    Key::Cancelled => "已取消，未做任何更改",
    Key::DryRunDone => "演练模式：未做任何更改",
    Key::DryRunNotApplicable => "--dry-run 不适用于 {0}",
    Key::PlanPayload => "  载荷：{0}，{1}",
    Key::PlanBackendInstall => "将通过 {0} 安装：{1}",
    Key::PlanRemoveFiles => "  将删除的文件：{0}",
    Key::PlanRemoveBackend => "  将通过 {0} 卸载：{1}",
    Key::PlanUpgrades => "将升级的软件包（{0} 个）：",
    Key::BatchTargets => "目标（{0} 个）：{1}",
    Key::BatchSummary => "完成：{0} 个成功，{1} 个失败",
    Key::BatchSomeFailed => "部分目标失败：{0}",

    Key::NotInstalled => "未安装：{0}",
    Key::LockBusy => "无法锁定 pica 数据库：{0} 已存在（持有者 pid {1}）",
    Key::LockUnverified => {
      "无法锁定 pica 数据库：{0} 已存在，本机无法确认 {1} 是否仍持有它，因此没有动这把锁；若确认没有其他 pica 在运行，删除该目录后重试"
    }
    Key::LockCleanupFailed => "无法锁定 pica 数据库：清理陈旧锁 {0} 失败：{1}",
    Key::LockFailed => "无法锁定 pica 数据库：{0}",
    Key::InvalidPkgname => "pkgname {0} 无效：须以字母或数字开头，只能使用字母、数字和 . _ + - @",
    Key::InvalidAppname => "appname {0} 无效：须以字母或数字开头，只能使用字母、数字和 . _ + -",
    Key::TooManyArguments => "{0} 只接受一个参数；多余的参数：{1}",
    Key::UnexpectedArgument => "{0} 不接受参数；多余的参数：{1}",
    Key::NoPackageManager => "未找到受支持的包管理器",
    Key::MissingCommand => "缺少必需的命令：{0}",
    Key::InvalidPkgmgr => "无效的 pkgmgr：{0}",
    Key::OptionalFlagsExclusive => "--with-optional 与 --no-optional 不能同时使用",
    Key::InvalidFeedPolicy => "无效的 --feed-policy：{0}",
    Key::InvalidPkgmgrFlag => "无效的 --pkgmgr：{0}",
    Key::InvalidLangFlag => "无效的 --lang：{0}",
    Key::MkdirFailed => "创建目录 {0} 失败：{1}",
    Key::InvalidApkForceSetting => {
      "{0} 中的 \"apk_force_non_repository\" 无效：应为 true 或 false，实际为 {1}"
    }
    Key::ConstraintUnmet => "{0} 要求 {1}:{2}，但已安装的是 {3}",
    Key::ConstraintUnmetInFeed => "{0} 要求 {1}:{2}，但软件源里最高只有 {3}",
    Key::DepPrecheckOtherBackend => "；本机还装有 {0}，本次只查了 {1}，可用 --pkgmgr {0} 重试",
    Key::LockBeingTaken => "一把正在获取中的锁",
    Key::LockWithoutPid => "一把没有 pid 的锁",
    Key::UnknownArg => "未知参数：{0}",
    Key::ArchFromUname => "没有包管理器报告架构列表；uname -m 报告 {0}",
    Key::ArchUnknown => "没有包管理器报告架构列表，uname -m 也无法读取",
    Key::NotAbsoluteUrl => "不是绝对 URL：{0}",
    Key::UrlWithCredentials => "拒绝使用带凭据的 URL：{0}",
    Key::RemoteSourceLocalUrl => "远程源不能从本机下载：{0}",
    Key::PlainHttpPublic => {
      "明文 HTTP 只用于本地源；{0} 是公网地址——请使用 https，确有需要时再为该源设置 allow_insecure_http"
    }
    Key::UnsupportedScheme => "不支持的 URL 协议 {0}：{1}",
    Key::RedirectDowngrade => "拒绝从 https 重定向到 {0}",
    Key::UnsafeRelativePath => "{0} 不是安全的相对路径：{1}",
    Key::SourceRootNotAbsolute => "源根地址不是绝对 URL：{0}",
    Key::RedirectNoLocation => "重定向缺少 Location 值",
    Key::RedirectProtocolRelative => "拒绝协议相对的重定向：{0}",
    Key::RedirectUnresolvable => "无法解析来自 {0} 的重定向",
    Key::NotPicaDatabase => "{0} 不是 pica 数据库：{1}",
    Key::DatabaseEncodeFailed => "无法编码数据库：{0}",
    Key::IndexMissing => "缺少索引：请先运行 'pica -S'（{0}）",
    Key::JsonRootNotObject => "JSON 根节点不是对象",
    Key::JsonFieldNotObject => "JSON 字段 '{0}' 不是对象",
    Key::CacheCleanupSkipped => "跳过缓存清理：无法读取 {0}：{1}",
    Key::CacheCleanupFailed => "缓存清理失败：{0}：{1}",
    Key::InvalidFetchTimeout => "无效的 --fetch-timeout：{0}",
    Key::ReadFileUrlFailed => "读取 file URL 失败：{0}",
    Key::NoFetchTool => "未找到下载工具（需要 uclient-fetch、wget 或 curl）",
    Key::TooManyRedirects => "重定向次数过多（>{0}），起始于 {1}",
    Key::DownloadFailed => "尝试 {0} 次后下载失败（超时 {1} 秒）：{2} 详情=[{3}]",
    Key::TempHeadersFailed => "创建响应头临时文件失败：{0}",
    Key::TempBodyFailed => "创建响应体临时文件失败：{0}",
    Key::CurlFailed => "curl 执行失败：{0}",
    Key::CurlDownloadFailed => "curl 下载失败：{0}",
    Key::ReadRedirectHeadersFailed => "读取重定向响应头失败：{0}",
    Key::HttpNoLocation => "HTTP {0} 缺少 Location 响应头：{1}",
    Key::HttpStatus => "HTTP {0}：{1}",
    Key::ReadBodyFailed => "读取下载内容失败：{0}",
    Key::WgetFailed => "wget 执行失败：{0}",
    Key::WgetDownloadFailed => "wget 下载失败：{0}",
    Key::CommandExited => "{0} 退出，状态 {1}",
    Key::TarFailed => "tar 解包失败：{0}",
    Key::TarExtractFailed => "tar 解包失败：{0}：{1}",
    Key::FetcherDownloadFailed => "{0} 下载失败：{1}",
    Key::HookNotFound => "找不到 {0} 钩子：{1}",
    Key::HookRunning => "正在执行 {0} 钩子：{1}",
    Key::UnknownError => "未知错误",
    Key::KeepingOwnedFile => "保留 {0}：仍属于 {1}",
    Key::IndexReposNotObject => "索引中的 repos 不是对象",
    Key::SyncDepCheckNoManager => "跳过依赖检查：未找到受支持的包管理器",
    Key::SyncDepCheckSkipped => "跳过依赖检查：{0}",
    Key::SyncFeedMissingDep => "{0} 软件源缺少依赖：{1}",
    Key::SyncFeedMissingSome => "部分声明的依赖在 {0} 软件源中缺失",
    Key::ConstraintsUnreadable => "{0}：无法读取 {1} 约束：{2}",
    Key::BackupUnsupportedKind => {
      "{0} 既不是文件、符号链接也不是目录；pica 无法恢复它，因此安装在写入任何内容之前停止"
    }
    Key::BackupRemakeDirFailed => "无法将 {0} 恢复为目录：{1}",
    Key::BackupStoreFailed => "无法为本次安装替换的文件创建备份目录（{0}）：{1}",
    Key::BackupRecreateFailed => "无法重建 {0}：{1}",
    Key::ReadFailed => "读取 {0} 失败：{1}",
    Key::BackupReadFailed => "替换前无法读取 {0}（{1}）；尚未写入任何内容",
    Key::BackupCopyFailed => "无法将 {0} 的副本保存到 {1}（{2}）；尚未写入任何内容",
    Key::OptionalIndexUnavailable => "依赖索引不可用，无法提供可选依赖：{0}",
    Key::CheckingDeps => "正在检查声明的依赖...",
    Key::DepPrecheckMissing => "依赖预检：缺少 {0}",
    Key::DependenciesUninstalled => "本机没有 opkg 或 apk，这些依赖未安装：{0}",
    Key::DepPrecheckFailed => "依赖预检失败，缺少：{0}{1}",
    Key::InstallingOptional => "按要求安装 {0} 个可选依赖",
    Key::DepIsPicaPackage => {
      "；{0} 存在于 Pica 源中，但 pica 不会把 Pica 软件包当作依赖解析——请改为声明系统包名，或把容器包放进本制品的 pkgs/"
    }
    Key::PicaArtifactWithPkgs => {
      "pica 后端的制品不能携带 {0}/：pica 制品只包含 {1}/ 和 scripts/，pica 不会把容器包交给 opkg 或 apk。请用 `targets = opkg` 或 `targets = apk` 重新打包，让设备的包管理器安装它们；或在打包前把它们解到 {2}/ 中"
    }
    Key::AppPhaseUnknownAdded => "无法确定 app 阶段新增了哪些软件包：{0}",
    Key::FeedOnlyNothingDeclared => {
      "feed-only 策略：制品没有声明软件包，且不能使用其自带的容器包（请声明它们，或换一个 --feed-policy 安装）"
    }
    Key::PackagedOnlyNoContainers => "packaged-only 策略：制品没有自带容器包，且不能使用软件源",
    Key::FeedsPrompt => {
      "在 {2} 软件源中找到 {0}/{1} 个声明的软件包。是否用软件源代替自带的容器包？"
    }
    Key::NoPackagesInDir => "{0} 目录中没有软件包：{1}",
    Key::GuardSymlinkUnknown => "无法确定符号链接 {0} 的指向：{1}",
    Key::GuardPathUnknown => "无法确定载荷路径 {0} 的落点：{1}",
    Key::GuardPicaState => "载荷会写入 Pica 自己的 {0}：{1}",
    Key::ClearFailed => "无法清空 {0}：{1}",
    Key::ScriptNotFound => "软件包中找不到脚本：{0}",
    Key::CopyScriptFailed => "复制脚本失败：{0}",
    Key::OptionalSkipped => "已跳过可选依赖（{0}）：{1}",
    Key::OptionalNoTerminal => {
      "pica：stdin 不是终端，不安装任何可选依赖（使用 --with-optional 可全部安装）"
    }
    Key::OptionalSelectPrompt => "输入编号或 `all` 选择可选软件包，直接回车跳过：",
    Key::OptionalTitle => "{0} 的可选依赖\n\n",
    Key::OptionalFromPkgs => "随本次请求从制品的 pkgs/ 安装",
    Key::OptionalAlreadyInstalled => "已安装",
    Key::OptionalInstalledUnmetReq => "已安装，但{0}（{1}）",
    Key::OptionalInstalledUnmet => "已安装，但{0}",
    Key::OptionalSizes => "下载：{0}  安装后：{1}",
    Key::OptionalNotNumber => "`{0}` 不是数字；未选择任何项",
    Key::OptionalNotOffered => "`{0}` 不在可选编号中；未选择任何项",
    Key::OptionalSizesUnknown => "可选依赖的大小未知：{0}",
    Key::CopyEnvFailed => "复制 env 文件失败：{0}",
    Key::ManifestMissing => "manifest 缺少 {0}",
    Key::InvalidManifestPkgmgr => "manifest 中的 pkgmgr 无效：{0}",
    Key::PkgmgrConflict => "--pkgmgr {0} 与软件包的 pkgmgr {1} 冲突",
    Key::HeaderInstalling => "正在安装 {0}（{1}）",
    Key::HeaderAlreadyInstalled => "{0} {1} 已安装；重新安装",
    Key::HeaderUpgrading => "正在升级 {0}（{1} -> {2}）",
    Key::HeaderDowngrading => "正在降级 {0}（{1} -> {2}）",
    Key::HeaderReinstalling => "正在重新安装 {0}（{1}）",
    Key::InstalledSummary => "已安装 {0}，{1}",
    Key::PkgfileNotFound => "找不到软件包文件：{0}",
    Key::PackageMissingManifest => "软件包缺少 manifest",
    Key::InvalidManifest => "manifest 无效：{0}",
    Key::ForeignContainer => {
      "{0} 是 {1} 软件包，不是 Pica 制品：请用 `{2} {3}` 安装，或把它放进 Pica 软件包的 pkgs/{4}/ 中"
    }
    Key::DepsChangedVersion => "本次安装过程中依赖的版本发生了变化，回滚无法恢复它们：{0}",
    Key::OptionalDepSkipped => "已跳过可选依赖 {0}：{1}",
    Key::InstalledVersionUnknown => "{0} 的已安装版本未知，因此未检查声明的 {1}",
    Key::ArchFamilyMatch => "架构 {0} 仅凭同族匹配被接受（{1}）",
    Key::ArchUnchecked => "无法确定本机的软件包架构，因此未检查 arch={0}（{1}）",
    Key::UnsupportedArch => "不支持的架构：pkg={0}（{1}）",
    Key::UnsupportedUname => "不支持的 uname：pkg={0} host={1}",
    Key::UnsupportedOs => "不支持的系统：pkg={0} host={1}",
    Key::PackageEmpty => "软件包为空：不含 root/ 或 scripts/，pkgs/ 中也没有软件包",
    Key::InvalidSourceAvailability => "无效的 source_availability 值：{0}（支持：{1}）",
    Key::LuciTypeNeedsVariant => "type=luci 需要 luci=<lua1|js2>",
    Key::InvalidLuci => "无效的 luci 值：{0}",
    Key::LuciUndetectable => "需要 luci 变体（{0}），但无法检测本机",
    Key::UnsupportedLuci => "不支持的 luci 变体：pkg={0} host={1}",
    Key::BackupCleanupFailed => "安装已完成，但被替换文件的备份无法从 {0} 删除（{1}）",
    Key::InstalledListUnreadable => "无法读取已安装软件包列表；回滚记录可能不完整：{0}",
    Key::RestoreFailed => "pica 无法恢复 {0}；保留的副本在 {1}，那是这些文件仅存的位置",
    Key::TheReplacedFiles => "被替换的文件",
    Key::ApkNoCacheDir => "没有索引缓存目录（{0}），涉及 {1} 个仓库",
    Key::NoIndexFile => "{0} 处没有索引文件",
    Key::NoCachedIndex => "没有 {0} 的缓存索引",
    Key::ApkSearchFailed => "apk search --exact {0} 失败：{1}",
    Key::ApkSearchExit => "apk search --exact {0} 失败（退出码 {1}）：{2}",
    Key::ApkSearchKilled => "apk search --exact {0} 被信号终止：{1}",
    Key::ApkAddFailed => "apk add 失败：{0}",
    Key::InstallStorageFull => "{0} 安装失败：{1}（存储空间已满）。详情=[{2}]",
    Key::BackendInstallFailed => "{0} 安装失败：{1} 详情=[{2}]",
    Key::ApkDelFailed => "apk del 失败：{0}",
    Key::ApkDelTargetFailed => "apk del 失败：{0} 详情=[{1}]",
    Key::ApkListFailed => "apk list --installed 失败：{0}",
    Key::ApkListExit => "apk list --installed 失败（{0}）：{1}",
    Key::ApkMixedRequest => "一次 apk 请求不能混用本地 apk 文件和软件包名",
    Key::InvalidLocalApk => "无效的本地 apk：{0}",
    Key::ApkTargetInvalid => "应为软件包名或已存在的本地 apk 文件",
    Key::ReadDirFailed => "读取目录 {0} 失败：{1}",
    Key::ReposNoKeyword => "缺少关键字或 URL",
    Key::ReposUnknownKeyword => "无法识别的关键字：{0}",
    Key::ReposNoUrl => "缺少 URL",
    Key::ReposInvalidUrl => "无效的 URL：{0}",
    Key::ReposInvalidOption => "无效的选项：{0}",
    Key::ReposInvalidVariable => "无效的变量定义：{0}",
    Key::ReposNoArch => "无法确定 {0} 需要哪个索引：{1} 没有指明机器类型",
    Key::ReposUnterminated => "变量未结束：{0}",
    Key::ReposVariableLength => "无效的变量长度：{0}",
    Key::ReposUndefined => "未定义的变量：{0}",
    Key::IndexReposFrom => "（仓库来自 {0}）",
    Key::IndexUnavailableDetail => "{0} 索引不可用：{1}{2}{3}",
    Key::KilledBySignal => "被信号终止",
    Key::UpdateFailedSilent => "{0} update 失败（{1}，无输出）",
    Key::UpdateFailedStatus => "{0} update 失败（{1}）：{2}",
    Key::NoRefreshCommand => "未配置索引刷新命令",
    Key::UpdateFailed => "{0} update 失败：{1}",
    Key::PackagesAndMore => "{0} 个软件包（{1}，另有 {2} 个）",
    Key::NotTextIndex => "{0} 不是文本索引，无法读取其条目",
    Key::NoFeedIndex => "软件源 {0} 没有索引（{1}）",
    Key::UpdateBlocked => "{0} update 被阻塞：{1} 被 pid {2} 占用",
    Key::OpkgInfoFailed => "opkg info {0} 失败：{1}",
    Key::OpkgInfoExit => "opkg info {0} 失败（{1}）：{2}",
    Key::OpkgInstallFailed => "opkg install 失败：{0}",
    Key::OpkgRemoveFailed => "opkg remove 失败：{0}",
    Key::OpkgRemoveTargetFailed => "opkg remove 失败：{0} 详情=[{1}]",
    Key::OpkgListFailed => "opkg list-installed 失败：{0}",
    Key::OpkgListExit => "opkg list-installed 失败（{0}）：{1}",
    Key::RequiresArgument => "{0} 需要参数 {1}",
    Key::WarningLine => "警告：{0}",
    Key::BoundUnmet => "已安装的版本不满足声明的约束",
    Key::OriginRecorded => "记录于 pkgmgr",
    Key::OriginLegacySource => "由旧版 source 字段推断",
    Key::OriginLegacyPkgfile => "由旧版 pkgfile 字段推断",
    Key::OriginDefault => "未记录后端",
    Key::GuardConfigFile => "配置文件",
    Key::GuardDatabase => "数据库",
    Key::GuardIndex => "索引",
    Key::GuardInstallReport => "安装报告",
    Key::GuardLockFile => "锁文件",
    Key::GuardLockDir => "锁目录",
    Key::GuardKeptScripts => "保留的脚本",
    Key::GuardEnvFragments => "env 片段",
    Key::GuardStateDir => "状态目录",
    Key::GuardConfigDir => "配置目录",
    Key::ReasonFromPkgs => "本制品从 pkgs/ 安装依赖",
    Key::ReasonNoSource => "本次安装没有依赖来源",
    Key::ReasonPackagedButFeeds => "制品中自带，但本次安装使用软件源",
    Key::ReasonFeedsUnreadable => "无法读取软件源",
    Key::ReasonNotInFeeds => "软件源中没有",
    Key::ReasonPicaCannotInstall => "pkgmgr=pica 无法安装依赖",
    Key::OptionalUnavailable => "不可用（{0}）",
    Key::SizeUnknown => "未知",
    Key::SizeEstimated => "约 {0}（估计）",
    Key::OptionalSizesNote => "\n大小仅为该软件包本身，取自软件源记录，不含其依赖。\n",
    Key::SizeInstalledLabel => "软件包安装后大小",
    Key::SizeDownloadLabel => "下载大小",
    Key::ApkNeedsForce => {
      "；在 apk 3.x 上，没有任何仓库提供的本地 apk 需要 --force-non-repository，而本次请求没有带上它（在 pica.json 中设置 \"apk_force_non_repository\": true）"
    }
    Key::ApkRejectsForce => {
      "；此 apk 不接受 --force-non-repository（在 pica.json 中设置 \"apk_force_non_repository\": false）"
    }
    Key::Usage => {
      "用法：\n  pica setup              交互式初始化：语言、源、后端、同步\n  pica -S [selector...]   同步仓库（不带 selector）或按 selector 安装（自动选择来源）\n  pica -Su                升级所有已安装的 pica 软件包\n  pica -Syu               先同步，再升级所有已安装的 pica 软件包\n  pica -Si <selector>     显示已同步索引中的远程软件包信息\n  pica -Ss <terms>        搜索已同步的源（--all：包含本机无法安装的软件包）\n  pica -Sl [source]       列出已同步的源发布的内容\n  pica -Qs <terms>        搜索已安装的软件包\n  pica -Qo <path>         显示文件属于哪个软件包\n  pica -Qu                列出有新版本可用的已安装软件包\n  pica -So <selector>     通过所选的系统包管理器安装\n  pica -Sp <selector...>  按 selector 安装（强制使用 pica 仓库）\n  pica -U <pkgfile|url>...\n                          从本地文件或 URL 安装/更新\n  pica -R <pkgname...>    卸载软件包（不处理依赖）\n  pica -Q                 列出已安装的 pica 软件包\n  pica -Qi <pkgname>      显示已安装软件包的信息\n  pica -Ql <pkgname>      列出已安装软件包的文件\n  pica --json ...         成功和失败都输出 JSON（需显式指定）\n  pica --json-errors ...  仅在失败时输出 JSON\n  pica --non-interactive ...\n                            禁用交互提示（供后端/自动化使用）\n  pica --dry-run ...      展示 -S/-Sp/-So/-U/-R/-Su 的执行计划，不做任何更改\n  pica --noconfirm ...    更改系统前不再询问（别名 --yes）\n  pica --feed-policy <mode>\n                            ask|feed-first|packaged-first|feed-only|packaged-only\n  pica --with-optional    安装所有可用的可选依赖\n  pica --no-optional      不安装任何可选依赖（非终端时的默认行为）\n  pica --pkgmgr <backend> opkg|apk|pica（优先于 manifest 和配置）\n  pica --lang <code>      本次运行的界面语言：zh-CN|en-US\n  pica -V\n  pica --version\n\n说明：\n  - 依赖：tar，以及 URL 安装/同步所需的下载工具之一（uclient-fetch/wget/curl）。\n  - 后端：apk、opkg、pica。选择顺序为 --pkgmgr、软件包 manifest、\n    /etc/pica/pica.json 中的 \"pkgmgr\"，最后自动检测（apk 优先于 opkg，都没有则用 pica）。\n  - 配置：/etc/pica/pica.json\n  - 状态：/var/lib/pica/db.json、/var/lib/pica/index.json\n  - 锁：  /var/lib/pica/db.lck\n  - selector 示例：app(branch)"
    }
    Key::InvalidOptionValue => "无效的 {0}：{1}",
    Key::ArchReportedBy => "{0} 报告 {1}",
    Key::CommandFailed => "{0} 执行失败：{1}",
    Key::IndexLabel => "{0} 索引",
    Key::PackagesList => "{0} 个软件包（{1}）",
    Key::ExitCode => "退出码 {0}",
  }
}

#[cfg(test)]
mod tests {
  use super::{Key, Lang, fill, tr};

  #[test]
  fn tr_speaks_the_language_made_current_and_takes_any_display() {
    Lang::ZhCn.make_current();
    assert_eq!(tr(Key::SyncUpdated, &[&"main"]), "main 已更新");
    Lang::EnUs.make_current();
    assert_eq!(
      tr(Key::UpgradeSummary, &[&1, &0, &2]),
      "Upgrade finished: 1 upgraded, 0 skipped, 2 failed"
    );
  }

  #[test]
  fn placeholders_are_filled_in_order() {
    assert_eq!(fill("{0} downloading...", &["main"]), "main downloading...");
    assert_eq!(fill("{0}: {1}", &["a", "b"]), "a: b");
    // A placeholder with no argument stays visible instead of vanishing.
    assert_eq!(fill("{2}", &["a"]), "{2}");
    // Braces that are not placeholders are not treated as ones.
    assert_eq!(fill("a { b", &[]), "a { b");
    assert_eq!(fill("100%", &[]), "100%");
  }

  #[test]
  fn every_key_says_something_in_both_languages() {
    for key in ALL_KEYS {
      assert!(!Lang::EnUs.text(*key).is_empty(), "{key:?} has no English text");
      assert!(!Lang::ZhCn.text(*key).is_empty(), "{key:?} has no text and no fallback");
    }
  }

  /// The highest placeholder index a template uses, plus one.
  fn arity(template: &str) -> usize {
    template
      .match_indices('{')
      .filter_map(|(start, _)| template[start + 1..].split('}').next())
      .filter_map(|token| token.parse::<usize>().ok())
      .max()
      .map_or(0, |index| index + 1)
  }

  #[test]
  fn a_translation_asks_for_the_same_arguments_as_the_original() {
    for key in ALL_KEYS {
      assert_eq!(
        arity(Lang::ZhCn.text(*key)),
        arity(Lang::EnUs.text(*key)),
        "{key:?}: {} vs {}",
        Lang::ZhCn.text(*key),
        Lang::EnUs.text(*key)
      );
    }
  }

  #[test]
  fn chinese_is_used_for_chinese_and_english_for_english() {
    assert_eq!(Lang::ZhCn.text(Key::UpgradeNone), "没有可升级的 pica 软件包");
    assert_eq!(Lang::EnUs.text(Key::UpgradeNone), "No pica packages to upgrade");
    assert_eq!(Lang::ZhCn.args(Key::SyncUpdated, &["main"]), "main 已更新");
  }

  #[test]
  fn a_language_code_is_read_loosely_and_unknown_ones_are_none() {
    for code in ["zh-CN", "zh-cn", "zh_CN", "zh", "ZH-HANS"] {
      assert_eq!(Lang::parse(code), Some(Lang::ZhCn), "{code}");
    }
    for code in ["en-US", "en", "EN_us"] {
      assert_eq!(Lang::parse(code), Some(Lang::EnUs), "{code}");
    }
    assert_eq!(Lang::parse("de-DE"), None);
    assert_eq!(Lang::parse(""), None);

    // An unknown stored value is the default, not an error.
    let conf = serde_json::json!({ "language": "de-DE" });
    assert_eq!(Lang::from_config(&conf), Lang::EnUs);
    let conf = serde_json::json!({ "language": "zh-CN" });
    assert_eq!(Lang::from_config(&conf), Lang::ZhCn);
    // The LuCI `i18n` key is not the interface language.
    let conf = serde_json::json!({ "i18n": "zh-cn" });
    assert_eq!(Lang::from_config(&conf), Lang::EnUs);
  }

  #[test]
  fn codes_round_trip_through_the_saved_form() {
    for lang in Lang::ALL {
      assert_eq!(Lang::parse(lang.code()), Some(lang), "{lang:?}");
    }
  }

  /// The calls whose string argument is text a person reads.
  const TEXT_CALLS: &[&str] = &[
    "format!",
    "CliError::new",
    "log_info",
    "log_warn",
    "println!",
    "eprintln!",
    "print!",
    "eprint!",
    "require_arg",
    "invalid",
    "write!",
    "writeln!",
  ];

  /// Every string literal outside comments, with its byte range. Line
  /// continuations are joined the way the compiler joins them.
  fn literals(src: &str) -> Vec<(usize, usize, String)> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
      let rest = &src[i..];
      if rest.starts_with("//") {
        i += rest.find('\n').unwrap_or(rest.len());
      } else if rest.starts_with("/*") {
        i += rest.find("*/").map_or(rest.len(), |end| end + 2);
      } else if rest.starts_with('r')
        && (i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_'))
        && rest[1..].trim_start_matches('#').starts_with('"')
      {
        let hashes = rest[1..].len() - rest[1..].trim_start_matches('#').len();
        let open = 2 + hashes;
        let close = format!("\"{}", "#".repeat(hashes));
        let end = rest[open..].find(&close).map_or(rest.len(), |at| open + at);
        out.push((i, i + end + close.len(), rest[open..end].to_string()));
        i += end + close.len();
      } else if bytes[i] == b'\'' {
        // A char literal is skipped whole, so `'"'` does not open a string; a
        // lifetime is one character.
        let chars: Vec<char> = rest.chars().take(4).collect();
        i += match chars.as_slice() {
          ['\'', '\\', _, '\'', ..] => 4,
          ['\'', c, '\'', ..] => 2 + c.len_utf8(),
          _ => 1,
        };
      } else if bytes[i] == b'"' {
        let mut j = i + 1;
        let mut value = String::new();
        while bytes[j] != b'"' {
          if bytes[j] == b'\\' {
            if bytes[j + 1] == b'\n' {
              j += 2;
              while bytes[j].is_ascii_whitespace() {
                j += 1;
              }
              continue;
            }
            value.push_str(&src[j..j + 2]);
            j += 2;
            continue;
          }
          let ch = src[j..].chars().next().expect("inside the source");
          value.push(ch);
          j += ch.len_utf8();
        }
        out.push((i, j + 1, value));
        i = j + 1;
      } else {
        i += src[i..].chars().next().map_or(1, char::len_utf8);
      }
    }
    out
  }

  /// Literals in those calls that hold a word and are still not text to
  /// translate: the program's name, a pid, size units, a repository path and
  /// the tags of a rollback's leftovers.
  const NOT_TEXT: &[&str] = &[
    "pica: {}",
    "pid {pid}",
    "{:.2} GiB",
    "{:.2} MiB",
    "{:.2} KiB",
    "local/{name} {version}",
    "hook:{label} ({})",
    "pkg:{name} ({})",
  ];

  /// Whether a literal reads as text: a word and a space. Field labels
  /// (`Name   : {}`) and anything without a space (a path, a file name, a
  /// key) are not.
  fn is_sentence(value: &str) -> bool {
    if value.contains("  : ") || !value.contains(' ') || NOT_TEXT.contains(&value) {
      return false;
    }
    let mut stripped = String::new();
    let mut depth = 0;
    for ch in value.chars() {
      match ch {
        '{' => depth += 1,
        '}' if depth > 0 => {
          depth -= 1;
          stripped.push(' ');
        }
        _ if depth == 0 => stripped.push(ch),
        _ => {}
      }
    }
    stripped
      .split(|ch: char| !(ch.is_ascii_alphabetic() || ch == '_'))
      .any(|word| word.len() >= 2 && word.starts_with(|ch: char| ch.is_ascii_alphabetic()))
  }

  /// The name of the call whose argument list holds `pos`, or "".
  fn enclosing(src: &str, pos: usize) -> &str {
    let bytes = src.as_bytes();
    let mut depth = 0usize;
    for k in (0..pos).rev() {
      match bytes[k] {
        b')' | b']' | b'}' => depth += 1,
        b'(' if depth == 0 => {
          let before = src[..k].trim_end();
          let start = before
            .rfind(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == ':' || ch == '!'))
            .map_or(0, |at| at + 1);
          return &before[start..];
        }
        b'[' | b'{' | b';' if depth == 0 => return "",
        b'(' | b'[' | b'{' => depth -= 1,
        _ => {}
      }
    }
    ""
  }

  /// The sentences written straight into the code of `src`.
  fn sentences_in(src: &str) -> Vec<(usize, String)> {
    let src = src.replace("\r\n", "\n");
    let end = src.find("\n#[cfg(test)]\nmod tests {").unwrap_or(src.len());
    let src = &src[..end];
    literals(src)
      .into_iter()
      .filter(|(start, stop, value)| {
        let call = enclosing(src, *start);
        let after = &src[*stop..];
        is_sentence(value)
          && (TEXT_CALLS.iter().any(|name| call.ends_with(name))
            || after.starts_with(".to_string()")
            || after.starts_with(".into()"))
      })
      .map(|(start, _, value)| (src[..start].matches('\n').count() + 1, value))
      .collect()
  }

  fn sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read src").flatten() {
      let path = entry.path();
      if path.is_dir() {
        if path.file_name().is_some_and(|name| name != "pipeline_tests") {
          sources(&path, out);
        }
      } else if path.extension().is_some_and(|ext| ext == "rs")
        && path.file_name().is_some_and(|name| name != "i18n.rs")
      {
        out.push(path);
      }
    }
  }

  #[test]
  fn the_sentence_scan_finds_a_sentence_and_nothing_else() {
    let src = "fn f(app: &mut App) {\n  app.log_info(\"Some English words\");\n  \
               let _ = format!(\"pid {pid}\");\n  let _ = format!(\"{name}.json\");\n  println!(\"Name            : {x}\");\n  \
               let _ = '\"'; // \"a comment that talks\"\n  Err(CliError::new(E, format!(\n    \
               \"Installing {0}\", x)))\n}\n";
    let found: Vec<String> = sentences_in(src).into_iter().map(|(_, text)| text).collect();
    assert_eq!(found, vec!["Some English words".to_string(), "Installing {0}".to_string()]);
  }

  /// Text a person reads goes through this table, so it can be translated;
  /// the code names a [`Key`]. Programs' interfaces (field labels, file names,
  /// JSON) and a third party's words (matched, not printed) are not sentences
  /// by the rule above, or are not in one of the calls it looks at.
  #[test]
  fn no_sentence_is_written_outside_this_table() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&root, &mut files);
    files.sort();
    let mut found = Vec::new();
    for path in files {
      let src = std::fs::read_to_string(&path).expect("read source");
      for (line, text) in sentences_in(&src) {
        let shown = path.strip_prefix(&root).unwrap_or(&path).display();
        found.push(format!("{shown}:{line}: {text}"));
      }
    }
    assert!(found.is_empty(), "add a Key for each of these://n{}", found.join("\n"));
  }

  /// Every key, so the coverage test above cannot pass by looking at a subset.
  const ALL_KEYS: &[Key] = &[
    Key::SetupTitle,
    Key::SetupNeedsTerminal,
    Key::SetupConfUnreadable,
    Key::SetupNotAnObject,
    Key::SetupCancelled,
    Key::SetupInvalidChoice,
    Key::SetupLanguagePrompt,
    Key::SetupSources,
    Key::SetupSourceLine,
    Key::SetupSourcePrompt,
    Key::SetupSourceOfficial,
    Key::SetupSourceCustom,
    Key::SetupSourceKeep,
    Key::SetupAllowHttp,
    Key::SetupYes,
    Key::SetupNo,
    Key::SetupSourceUrl,
    Key::SetupUrlEmpty,
    Key::SetupOfficialMissing,
    Key::SetupNoCommonBackend,
    Key::SetupBackends,
    Key::SetupSaved,
    Key::SetupComplete,
    Key::SetupNextSteps,
    Key::SetupNotJson,
    Key::SyncStarted,
    Key::SyncDownloading,
    Key::SyncUpdated,
    Key::SyncPackageSkipped,
    Key::SyncNoRepos,
    Key::SyncReposNotArray,
    Key::SyncRepoMissingName,
    Key::SyncRepoMissingUrl,
    Key::SyncBackendsMissing,
    Key::SyncBackendsNotArray,
    Key::SyncUnsupportedBackend,
    Key::SyncBackendsEmpty,
    Key::SyncRootRejected,
    Key::SyncFetchFailed,
    Key::SyncNotUtf8,
    Key::SyncEntryInvalid,
    Key::SyncMissingBackend,
    Key::SyncIndexInvalid,
    Key::CommitReadFailed,
    Key::CommitRestored,
    Key::CommitBroken,
    Key::CommitFailed,
    Key::NotFound,
    Key::NotFoundHintSources,
    Key::NotFoundHintRepos,
    Key::Provides,
    Key::FileOne,
    Key::FileMany,
    Key::InstallBackendNoManager,
    Key::InstallPackageNotFound,
    Key::InstallPicaTooOld,
    Key::InstallInvalidFilename,
    Key::InstallDownloading,
    Key::InstallDownloadingUrl,
    Key::InstallSourceInvalid,
    Key::InstallVerifying,
    Key::InstallShaMismatch,
    Key::InstallInvalidSha,
    Key::InstallInvalidDownloadUrl,
    Key::InstallRollingBack,
    Key::InstallRolledBack,
    Key::InstallRollbackLeftovers,
    Key::InstallRollbackRecordFailed,
    Key::InstallRollbackIncomplete,
    Key::InstallReportFailed,
    Key::TransactionCompleted,
    Key::RemoveHeader,
    Key::RemoveHeaderVersioned,
    Key::RemovePackage,
    Key::RemoveSummary,
    Key::RemoveSummaryOnly,
    Key::RemoveSummaryOwned,
    Key::RemoveBackendInferred,
    Key::RemoveSkipScript,
    Key::RemoveRunScript,
    Key::UpgradeNone,
    Key::UpgradeSkip,
    Key::UpgradeUpToDate,
    Key::UpgradeAllUpToDate,
    Key::InstallOrphanKept,
    Key::InstallOrphansRemoved,
    Key::QueryNoOwner,
    Key::QueryOwnedBy,
    Key::SearchEmptyQuery,
    Key::SearchUnknownSource,
    Key::SearchNotSynced,
    Key::SearchInstalled,
    Key::SearchNotInstallable,
    Key::SearchHidden,
    Key::IndexStale,
    Key::NotFoundSuggest,
    Key::UpgradeFailed,
    Key::UpgradeSummary,
    Key::UpgradeSomeFailed,
    Key::ConfirmProceed,
    Key::Cancelled,
    Key::DryRunDone,
    Key::DryRunNotApplicable,
    Key::PlanPayload,
    Key::PlanBackendInstall,
    Key::PlanRemoveFiles,
    Key::PlanRemoveBackend,
    Key::PlanUpgrades,
    Key::BatchTargets,
    Key::BatchSummary,
    Key::BatchSomeFailed,
    Key::NotInstalled,
    Key::LockBusy,
    Key::LockCleanupFailed,
    Key::LockUnverified,
    Key::LockFailed,
    Key::InvalidPkgname,
    Key::InvalidAppname,
    Key::TooManyArguments,
    Key::UnexpectedArgument,
    Key::NoPackageManager,
    Key::MissingCommand,
    Key::InvalidPkgmgr,
    Key::OptionalFlagsExclusive,
    Key::InvalidFeedPolicy,
    Key::InvalidPkgmgrFlag,
    Key::InvalidLangFlag,
    Key::MkdirFailed,
    Key::InvalidApkForceSetting,
    Key::ConstraintUnmet,
    Key::ConstraintUnmetInFeed,
    Key::DepPrecheckOtherBackend,
    Key::LockBeingTaken,
    Key::LockWithoutPid,
    Key::UnknownArg,
    Key::ArchFromUname,
    Key::ArchUnknown,
    Key::NotAbsoluteUrl,
    Key::UrlWithCredentials,
    Key::RemoteSourceLocalUrl,
    Key::PlainHttpPublic,
    Key::UnsupportedScheme,
    Key::RedirectDowngrade,
    Key::UnsafeRelativePath,
    Key::SourceRootNotAbsolute,
    Key::RedirectNoLocation,
    Key::RedirectProtocolRelative,
    Key::RedirectUnresolvable,
    Key::NotPicaDatabase,
    Key::DatabaseEncodeFailed,
    Key::IndexMissing,
    Key::JsonRootNotObject,
    Key::JsonFieldNotObject,
    Key::CacheCleanupSkipped,
    Key::CacheCleanupFailed,
    Key::InvalidFetchTimeout,
    Key::ReadFileUrlFailed,
    Key::NoFetchTool,
    Key::TooManyRedirects,
    Key::DownloadFailed,
    Key::TempHeadersFailed,
    Key::TempBodyFailed,
    Key::CurlFailed,
    Key::CurlDownloadFailed,
    Key::ReadRedirectHeadersFailed,
    Key::HttpNoLocation,
    Key::HttpStatus,
    Key::ReadBodyFailed,
    Key::WgetFailed,
    Key::WgetDownloadFailed,
    Key::CommandExited,
    Key::TarFailed,
    Key::TarExtractFailed,
    Key::FetcherDownloadFailed,
    Key::HookNotFound,
    Key::HookRunning,
    Key::UnknownError,
    Key::KeepingOwnedFile,
    Key::IndexReposNotObject,
    Key::SyncDepCheckNoManager,
    Key::SyncDepCheckSkipped,
    Key::SyncFeedMissingDep,
    Key::SyncFeedMissingSome,
    Key::ConstraintsUnreadable,
    Key::BackupUnsupportedKind,
    Key::BackupRemakeDirFailed,
    Key::BackupStoreFailed,
    Key::BackupRecreateFailed,
    Key::ReadFailed,
    Key::BackupReadFailed,
    Key::BackupCopyFailed,
    Key::OptionalIndexUnavailable,
    Key::CheckingDeps,
    Key::DepPrecheckMissing,
    Key::DependenciesUninstalled,
    Key::DepPrecheckFailed,
    Key::InstallingOptional,
    Key::DepIsPicaPackage,
    Key::PicaArtifactWithPkgs,
    Key::AppPhaseUnknownAdded,
    Key::FeedOnlyNothingDeclared,
    Key::PackagedOnlyNoContainers,
    Key::FeedsPrompt,
    Key::NoPackagesInDir,
    Key::GuardSymlinkUnknown,
    Key::GuardPathUnknown,
    Key::GuardPicaState,
    Key::ClearFailed,
    Key::ScriptNotFound,
    Key::CopyScriptFailed,
    Key::OptionalSkipped,
    Key::OptionalNoTerminal,
    Key::OptionalSelectPrompt,
    Key::OptionalTitle,
    Key::OptionalFromPkgs,
    Key::OptionalAlreadyInstalled,
    Key::OptionalInstalledUnmetReq,
    Key::OptionalInstalledUnmet,
    Key::OptionalSizes,
    Key::OptionalNotNumber,
    Key::OptionalNotOffered,
    Key::OptionalSizesUnknown,
    Key::CopyEnvFailed,
    Key::ManifestMissing,
    Key::InvalidManifestPkgmgr,
    Key::PkgmgrConflict,
    Key::HeaderInstalling,
    Key::HeaderAlreadyInstalled,
    Key::HeaderUpgrading,
    Key::HeaderDowngrading,
    Key::HeaderReinstalling,
    Key::InstalledSummary,
    Key::PkgfileNotFound,
    Key::PackageMissingManifest,
    Key::InvalidManifest,
    Key::ForeignContainer,
    Key::DepsChangedVersion,
    Key::OptionalDepSkipped,
    Key::InstalledVersionUnknown,
    Key::ArchFamilyMatch,
    Key::ArchUnchecked,
    Key::UnsupportedArch,
    Key::UnsupportedUname,
    Key::UnsupportedOs,
    Key::PackageEmpty,
    Key::InvalidSourceAvailability,
    Key::LuciTypeNeedsVariant,
    Key::InvalidLuci,
    Key::LuciUndetectable,
    Key::UnsupportedLuci,
    Key::BackupCleanupFailed,
    Key::InstalledListUnreadable,
    Key::RestoreFailed,
    Key::TheReplacedFiles,
    Key::ApkNoCacheDir,
    Key::NoIndexFile,
    Key::NoCachedIndex,
    Key::ApkSearchFailed,
    Key::ApkSearchExit,
    Key::ApkSearchKilled,
    Key::ApkAddFailed,
    Key::InstallStorageFull,
    Key::BackendInstallFailed,
    Key::ApkDelFailed,
    Key::ApkDelTargetFailed,
    Key::ApkListFailed,
    Key::ApkListExit,
    Key::ApkMixedRequest,
    Key::InvalidLocalApk,
    Key::ApkTargetInvalid,
    Key::ReadDirFailed,
    Key::ReposNoKeyword,
    Key::ReposUnknownKeyword,
    Key::ReposNoUrl,
    Key::ReposInvalidUrl,
    Key::ReposInvalidOption,
    Key::ReposInvalidVariable,
    Key::ReposNoArch,
    Key::ReposUnterminated,
    Key::ReposVariableLength,
    Key::ReposUndefined,
    Key::IndexReposFrom,
    Key::IndexUnavailableDetail,
    Key::KilledBySignal,
    Key::UpdateFailedSilent,
    Key::UpdateFailedStatus,
    Key::NoRefreshCommand,
    Key::UpdateFailed,
    Key::PackagesAndMore,
    Key::NotTextIndex,
    Key::NoFeedIndex,
    Key::UpdateBlocked,
    Key::OpkgInfoFailed,
    Key::OpkgInfoExit,
    Key::OpkgInstallFailed,
    Key::OpkgRemoveFailed,
    Key::OpkgRemoveTargetFailed,
    Key::OpkgListFailed,
    Key::OpkgListExit,
    Key::RequiresArgument,
    Key::WarningLine,
    Key::BoundUnmet,
    Key::OriginRecorded,
    Key::OriginLegacySource,
    Key::OriginLegacyPkgfile,
    Key::OriginDefault,
    Key::GuardConfigFile,
    Key::GuardDatabase,
    Key::GuardIndex,
    Key::GuardInstallReport,
    Key::GuardLockFile,
    Key::GuardLockDir,
    Key::GuardKeptScripts,
    Key::GuardEnvFragments,
    Key::GuardStateDir,
    Key::GuardConfigDir,
    Key::ReasonFromPkgs,
    Key::ReasonNoSource,
    Key::ReasonPackagedButFeeds,
    Key::ReasonFeedsUnreadable,
    Key::ReasonNotInFeeds,
    Key::ReasonPicaCannotInstall,
    Key::OptionalUnavailable,
    Key::SizeUnknown,
    Key::SizeEstimated,
    Key::OptionalSizesNote,
    Key::SizeInstalledLabel,
    Key::SizeDownloadLabel,
    Key::ApkNeedsForce,
    Key::ApkRejectsForce,
    Key::Usage,
    Key::InvalidOptionValue,
    Key::ArchReportedBy,
    Key::CommandFailed,
    Key::IndexLabel,
    Key::PackagesList,
    Key::ExitCode,
  ];
}
