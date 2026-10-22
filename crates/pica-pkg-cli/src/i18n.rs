//! The text Pica prints to a person.
//!
//! One table per language, keyed by [`Key`], filled by [`Lang::text`] and
//! [`Lang::args`]. English is the fallback for a language that does not know a
//! key, and it is the default language, so a machine that never configures one
//! sees exactly the output it saw before this module existed.
//!
//! What is deliberately *not* in here:
//!
//! - `--help` / `-V` output, JSON payloads, `-Q`/`-Qi`/`-Ql`/`-Si` fields and
//!   error codes. Those are interfaces other programs read, so they stay in one
//!   shape whatever the language is.
//! - Package names, command names, config keys, protocol fields and paths.
//! - The words a third-party tool said. A message may be wrapped in a
//!   translated sentence, but the original diagnostic is kept verbatim.
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
  SetupLanguagePrompt,
  SetupSourceMenuPrompt,
  SetupSourceUrlNew,
  SetupUrlEmpty,
  SetupAddAnother,
  SetupConfUnreadable,
  SetupWriteFailed,
  SetupRestoreFailed,
  SetupInvalidChoice,
  SetupCancelled,
  SetupNeedsTerminal,
  SetupSources,
  SetupSourceMenuOfficial,
  SetupSourceMenuCustom,
  SetupSourceMenuAdd,
  SetupSourceMenuEdit,
  SetupSourceMenuKeep,
  SetupSourceLine,
  SetupPickSource,
  SetupSourceName,
  SetupSourceUrl,
  SetupOfficialUnavailable,
  SetupUrlRejected,
  SetupAllowHttp,
  SetupFetchFailed,
  SetupDescriptorInvalid,
  SetupBackendDevice,
  SetupBackendsDeclared,
  SetupBackendsPrompt,
  SetupUnknownBackend,
  SetupNoBackends,
  SetupSaved,
  SetupSyncFailed,
  SetupComplete,
  SetupNextSteps,
  SetupNotJson,
  SetupRootIsRelative,
  // ---- sync --------------------------------------------------------------
  SyncStarted,
  SyncDownloading,
  SyncUpdated,
  SyncNoRepos,
  SyncReposNotArray,
  SyncRepoMissingName,
  SyncRepoMissingUrl,
  SyncBackendsNotArray,
  SyncUnsupportedBackend,
  SyncBackendsEmpty,
  SyncRootRejected,
  SyncFetchFailed,
  SyncNotUtf8,
  SyncRepoInvalid,
  SyncEntryInvalid,
  SyncMissingBackend,
  SyncFallback,
  SyncIndexInvalid,
  // ---- query -------------------------------------------------------------
  NotFound,
  NotFoundHintSources,
  NotFoundHintRepos,
  NotFoundHintLegacy,
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
  // ---- shared errors -----------------------------------------------------
  NotInstalled,
  LockBusy,
  LockCleanupFailed,
  LockFailed,
}

/// The English wording. Exhaustive: every key has a string here.
#[allow(
  clippy::too_many_lines,
  reason = "one arm per phrase; splitting the table would only hide where a phrase lives"
)]
fn english(key: Key) -> &'static str {
  match key {
    Key::SetupTitle => "Setup Pica",
    Key::SetupLanguagePrompt => "Select language [1-2]: ",
    Key::SetupSourceMenuPrompt => "Select an option [1-{0}]: ",
    Key::SetupSourceUrlNew => "Source root URL: ",
    Key::SetupUrlEmpty => "A source root URL is required.",
    Key::SetupAddAnother => "Add or change another source? [y/N]: ",
    Key::SetupConfUnreadable => {
      "Cannot read {0}: {1}. Fix or remove it, then run pica setup again."
    }
    Key::SetupWriteFailed => "Cannot save the configuration: {0}",
    Key::SetupRestoreFailed => "{0}; restoring the previous files failed too: {1}",
    Key::SetupInvalidChoice => "Invalid choice: pick a number from {0} to {1}.",
    Key::SetupCancelled => "Setup cancelled; no configuration was written.",
    Key::SetupNeedsTerminal => {
      "pica setup needs an interactive terminal (stdin is not a tty). Write the configuration \
       directly and sync it: pica.json has \"language\" (zh-CN or en-US) and \"repos\" entries \
       with \"url\" and \"backends\"; then run `pica -S`."
    }
    Key::SetupSources => "Software sources",
    Key::SetupSourceMenuOfficial => "Official source",
    Key::SetupSourceMenuCustom => "Custom source (enter a root URL)",
    Key::SetupSourceMenuAdd => "Add a new source",
    Key::SetupSourceMenuEdit => "Edit an existing source",
    Key::SetupSourceMenuKeep => "Keep the current sources and finish",
    Key::SetupSourceLine => "  {0}. {1}  {2}  (backends: {3})",
    Key::SetupPickSource => "Select a source [1-{0}]: ",
    Key::SetupSourceName => "Source name (empty = {0}): ",
    Key::SetupSourceUrl => "Source root URL (empty keeps {0}): ",
    Key::SetupOfficialUnavailable => {
      "This build has no official source configured; enter a source root URL instead."
    }
    Key::SetupUrlRejected => "Source rejected: {0}",
    Key::SetupAllowHttp => "{0} is public and uses plain HTTP. Allow it for this source? [y/N]: ",
    Key::SetupFetchFailed => "Cannot read {0}: {1}",
    Key::SetupDescriptorInvalid => "index.json at {0} is not a valid source descriptor: {1}",
    Key::SetupBackendDevice => "This device uses the {0} backend.",
    Key::SetupBackendsDeclared => "The source declares these backends: {0}",
    Key::SetupBackendsPrompt => {
      "Backends to enable for this source (comma separated, empty = {0}): "
    }
    Key::SetupUnknownBackend => "{0} is not declared by this source (declared: {1})",
    Key::SetupNoBackends => "Enable at least one backend.",
    Key::SetupSaved => "Configuration saved to {0}.",
    Key::SetupSyncFailed => {
      "Sync failed, so the previous configuration was restored and nothing was changed: {0}"
    }
    Key::SetupComplete => "Setup complete: language {0}; sources: {1}.",
    Key::SetupNextSteps => {
      "Next: `pica -Si <package>` shows a package, `pica -S <package>` installs it, `pica -S` \
       re-syncs the sources."
    }
    Key::SetupNotJson => {
      "pica setup is interactive and does not support --json or --json-errors; run it without them"
    }
    Key::SetupRootIsRelative => "A source root must be an absolute URL, not {0}",

    Key::SyncStarted => "Synchronizing package databases...",
    Key::SyncDownloading => "{0} downloading...",
    Key::SyncUpdated => "{0} updated",
    Key::SyncNoRepos => {
      "no repos configured in {0}; add at least one entry with \"url\" and \"backends\""
    }
    Key::SyncReposNotArray => "config {0} is not an array",
    Key::SyncRepoMissingName => "repo[{0}] missing name",
    Key::SyncRepoMissingUrl => "repo[{0}] missing url",
    Key::SyncBackendsNotArray => "repo[{0}] ({1}) backends must be an array of backend names",
    Key::SyncUnsupportedBackend => {
      "repo[{0}] ({1}) backends contains unsupported backend {2} (supported: {3})"
    }
    Key::SyncBackendsEmpty => "repo[{0}] ({1}) backends is empty; name at least one backend",
    Key::SyncRootRejected => "{0}: source root {1}: {2}",
    Key::SyncFetchFailed => "{0}: fetching {1} from {2} failed: {3}",
    Key::SyncNotUtf8 => "{0}: {1} at {2} is not UTF-8",
    Key::SyncRepoInvalid => "{0}: repo.json at {1} failed strict schema/filename validation: {2}",
    Key::SyncEntryInvalid => "{0}: index.json at {1} is invalid: {2}",
    Key::SyncMissingBackend => {
      "{0}: source does not provide backend(s) {1} (declares: {2}); adjust its backends list or \
       use a source that does"
    }
    Key::SyncFallback => "{0}: repo.json unavailable, using index.json (declares: {1})",
    Key::SyncIndexInvalid => "{0}: {1} index at {2} is invalid: {3}",

    Key::NotFound => "package not found in pica repos: {0}{1}",
    Key::NotFoundHintSources => " (backend {0}; no synced sources, run -S first)",
    Key::NotFoundHintRepos => " (backend {0}; {1})",
    Key::NotFoundHintLegacy => "every backend (schema 2)",
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

    Key::NotInstalled => "not installed: {0}",
    Key::LockBusy => "cannot lock pica database: {0} exists (held by pid {1})",
    Key::LockCleanupFailed => "cannot lock pica database: stale lock cleanup failed for {0}: {1}",
    Key::LockFailed => "cannot lock pica database: {0}",
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
    Key::SetupLanguagePrompt => "选择语言 [1-2]：",
    Key::SetupSourceMenuPrompt => "请选择 [1-{0}]：",
    Key::SetupSourceUrlNew => "软件源根地址：",
    Key::SetupUrlEmpty => "必须填写软件源根地址。",
    Key::SetupAddAnother => "继续添加或修改其他软件源？[y/N]：",
    Key::SetupConfUnreadable => "无法读取 {0}：{1}。请修复或移走它后重新运行 pica setup。",
    Key::SetupWriteFailed => "无法保存配置：{0}",
    Key::SetupRestoreFailed => "{0}；恢复原有文件同样失败：{1}",
    Key::SetupInvalidChoice => "无效选择：请输入 {0} 到 {1} 之间的数字。",
    Key::SetupCancelled => "已取消初始化，未写入任何配置。",
    Key::SetupNeedsTerminal => {
      "pica setup 需要交互式终端（stdin 不是 tty）。请直接写配置文件后同步：pica.json 中的 \
       \"language\"（zh-CN 或 en-US）与 \"repos\" 条目（\"url\"、\"backends\"），再运行 `pica -S`。"
    }
    Key::SetupSources => "软件源",
    Key::SetupSourceMenuOfficial => "官方源",
    Key::SetupSourceMenuCustom => "自定义源（输入根地址）",
    Key::SetupSourceMenuAdd => "添加新软件源",
    Key::SetupSourceMenuEdit => "修改已有软件源",
    Key::SetupSourceMenuKeep => "保留当前软件源并结束",
    Key::SetupSourceLine => "  {0}. {1}  {2}  （后端：{3}）",
    Key::SetupPickSource => "选择软件源 [1-{0}]：",
    Key::SetupSourceName => "软件源名称（留空 = {0}）：",
    Key::SetupSourceUrl => "软件源根地址（留空保持 {0}）：",
    Key::SetupOfficialUnavailable => "此构建未配置官方源，请改填软件源根地址。",
    Key::SetupUrlRejected => "软件源被拒绝：{0}",
    Key::SetupAllowHttp => "{0} 是公网地址且使用明文 HTTP。为此源放行？[y/N]：",
    Key::SetupFetchFailed => "无法读取 {0}：{1}",
    Key::SetupDescriptorInvalid => "{0} 的 index.json 不是有效的源入口描述：{1}",
    Key::SetupBackendDevice => "本机使用的后端是 {0}。",
    Key::SetupBackendsDeclared => "该源声明了这些后端：{0}",
    Key::SetupBackendsPrompt => "为该源启用的后端（逗号分隔，留空 = {0}）：",
    Key::SetupUnknownBackend => "{0} 未被该源声明（已声明：{1}）",
    Key::SetupNoBackends => "至少启用一个后端。",
    Key::SetupSaved => "配置已保存到 {0}。",
    Key::SetupSyncFailed => "同步失败，已恢复原有配置，未做任何改动：{0}",
    Key::SetupComplete => "初始化完成：语言 {0}；软件源：{1}。",
    Key::SetupNextSteps => {
      "接下来：`pica -Si <包名>` 查看软件包，`pica -S <包名>` 安装，`pica -S` 重新同步软件源。"
    }
    Key::SetupNotJson => "pica setup 是交互式命令，不支持 --json / --json-errors，请去掉后重试。",
    Key::SetupRootIsRelative => "软件源根地址必须是绝对 URL，而不是 {0}",

    Key::SyncStarted => "正在同步软件源……",
    Key::SyncDownloading => "正在下载 {0}...",
    Key::SyncUpdated => "{0} 已更新",
    Key::SyncNoRepos => "未在 {0} 中配置软件源；请至少添加一个含 \"url\" 与 \"backends\" 的条目",
    Key::SyncReposNotArray => "配置项 {0} 不是数组",
    Key::SyncRepoMissingName => "repo[{0}] 缺少 name",
    Key::SyncRepoMissingUrl => "repo[{0}] 缺少 url",
    Key::SyncBackendsNotArray => "repo[{0}]（{1}）的 backends 必须是后端名数组",
    Key::SyncUnsupportedBackend => "repo[{0}]（{1}）的 backends 含不支持的后端 {2}（支持：{3}）",
    Key::SyncBackendsEmpty => "repo[{0}]（{1}）的 backends 为空；至少写一个后端",
    Key::SyncRootRejected => "{0}：软件源根地址 {1}：{2}",
    Key::SyncFetchFailed => "{0}：从 {2} 获取 {1} 失败：{3}",
    Key::SyncNotUtf8 => "{0}：{2} 的 {1} 不是 UTF-8",
    Key::SyncRepoInvalid => "{0}：{1} 的 repo.json 未通过严格的 schema/filename 校验：{2}",
    Key::SyncEntryInvalid => "{0}：{1} 的 index.json 无效：{2}",
    Key::SyncMissingBackend => {
      "{0}：软件源未提供后端 {1}（已声明：{2}）；请调整 backends 或改用提供该后端的源"
    }
    Key::SyncFallback => "{0}：repo.json 不可用，改用 index.json（声明：{1}）",
    Key::SyncIndexInvalid => "{0}：{2} 的 {1} 索引无效：{3}",

    Key::NotFound => "pica 软件源中未找到软件包：{0}{1}",
    Key::NotFoundHintSources => "（后端 {0}；尚未同步软件源，请先运行 -S）",
    Key::NotFoundHintRepos => "（后端 {0}；{1}）",
    Key::NotFoundHintLegacy => "所有后端（schema 2）",
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

    Key::NotInstalled => "未安装：{0}",
    Key::LockBusy => "无法锁定 pica 数据库：{0} 已存在（持有者 pid {1}）",
    Key::LockCleanupFailed => "无法锁定 pica 数据库：清理陈旧锁 {0} 失败：{1}",
    Key::LockFailed => "无法锁定 pica 数据库：{0}",
  }
}

#[cfg(test)]
mod tests {
  use super::{Key, Lang, fill};

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

  /// Every key, so the coverage test above cannot pass by looking at a subset.
  /// Every key, so the coverage test above cannot pass by looking at a subset.
  const ALL_KEYS: &[Key] = &[
    Key::SetupTitle,
    Key::SetupLanguagePrompt,
    Key::SetupSourceMenuPrompt,
    Key::SetupSourceUrlNew,
    Key::SetupUrlEmpty,
    Key::SetupAddAnother,
    Key::SetupConfUnreadable,
    Key::SetupWriteFailed,
    Key::SetupRestoreFailed,
    Key::SetupInvalidChoice,
    Key::SetupCancelled,
    Key::SetupNeedsTerminal,
    Key::SetupSources,
    Key::SetupSourceMenuOfficial,
    Key::SetupSourceMenuCustom,
    Key::SetupSourceMenuAdd,
    Key::SetupSourceMenuEdit,
    Key::SetupSourceMenuKeep,
    Key::SetupSourceLine,
    Key::SetupPickSource,
    Key::SetupSourceName,
    Key::SetupSourceUrl,
    Key::SetupOfficialUnavailable,
    Key::SetupUrlRejected,
    Key::SetupAllowHttp,
    Key::SetupFetchFailed,
    Key::SetupDescriptorInvalid,
    Key::SetupBackendDevice,
    Key::SetupBackendsDeclared,
    Key::SetupBackendsPrompt,
    Key::SetupUnknownBackend,
    Key::SetupNoBackends,
    Key::SetupSaved,
    Key::SetupSyncFailed,
    Key::SetupComplete,
    Key::SetupNextSteps,
    Key::SetupNotJson,
    Key::SetupRootIsRelative,
    Key::SyncStarted,
    Key::SyncDownloading,
    Key::SyncUpdated,
    Key::SyncNoRepos,
    Key::SyncReposNotArray,
    Key::SyncRepoMissingName,
    Key::SyncRepoMissingUrl,
    Key::SyncBackendsNotArray,
    Key::SyncUnsupportedBackend,
    Key::SyncBackendsEmpty,
    Key::SyncRootRejected,
    Key::SyncFetchFailed,
    Key::SyncNotUtf8,
    Key::SyncRepoInvalid,
    Key::SyncEntryInvalid,
    Key::SyncMissingBackend,
    Key::SyncFallback,
    Key::SyncIndexInvalid,
    Key::NotFound,
    Key::NotFoundHintSources,
    Key::NotFoundHintRepos,
    Key::NotFoundHintLegacy,
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
    Key::NotInstalled,
    Key::LockBusy,
    Key::LockCleanupFailed,
    Key::LockFailed,
  ];
}
