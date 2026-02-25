# Pica TODO

## P0 - Critical

### 拆分 main.rs (God Object)

`crates/pica-cli-rs/src/main.rs` 848 行，承载了 CLI 路由、仓库搜索、依赖预检、IPK 安装、用户交互、平台检测、钩子执行、文件工具、manifest 辅助等职责。

建议拆分：
- `platform.rs` — `detect_platform`, `detect_opkg_arches`, `detect_luci_variant`, `normalize_uname`
- `precheck.rs` — `build_precheck_report`, `precheck_assert_no_missing`, `precheck_dep_source`
- `feed.rs` — `should_use_feeds`, `install_via_feeds_or_ipk`, `install_ipk_dir`
- `index.rs` — `find_pica_candidates_in_index`, `RepoCandidate`
- `hook.rs` — `run_hook`
- `util.rs` — `write_file_atomic`, `ensure_dir`, `prompt_yn`, `reorder_app_list`, `pkg_list_diff_added`, `canonicalize_display`

### 统一错误类型

当前存在三套结构相同但互不兼容的错误类型：
- `pica-core::error::PicaError` (enum: Message/Io/Json)
- `types.rs::CliError` {code, message}
- `lock.rs::LockError` {code, message}

`CliError` 和 `LockError` 完全同构却各自独立。`map_core_error` 手动桥接丢失结构化信息。

建议：合并 `LockError` 到 `CliError`，为 `PicaError` 实现 `From<PicaError> for CliError`。

### 实现包完整性校验

- `fetch_url` 下载后直接写盘安装，无任何 hash 校验
- `repo.json` 文档已预留 `md5`/`size` 字段，代码未实现
- hook 脚本以 root 权限通过 `sh` 执行无沙箱
- HTTP 场景存在 MITM 攻击面

至少实现 `sha256` 校验（推荐替代 md5），在 `repo.json` schema 中添加 `sha256` 字段。

## P1 - High

### 消除代码重复

**`write_json_atomic_pretty`** 存在 3 份近似实现：
- `types.rs:287-317`
- `state.rs:182-212`
- `pica-core/src/io.rs:28-41`

**`now_unix_secs`** 存在 4 份相同实现：
- `state.rs:233`
- `commands/sync.rs:193`
- `commands/install.rs:536`
- `pica-core/src/io.rs:9`

**`manifest_get_first/scalar/array`** 在 `main.rs` 和 `pica-core/manifest.rs` 各有一套。

**`ensure_dir`** 分散在 `main.rs`, `types.rs`, `lock.rs`, `pica-core/io.rs`。

应统一收敛到 `pica-core`，CLI 只做调用方。

### 清理 Bash 双维护

`legacy/pica-cli/pica` (~1700 行) 与 `pica-rs` 功能完整重叠，每次 bug fix / feature 需改两处。
行为存在细微差异（如 Bash 版用 `flock` + fallback，Rust 版只用目录锁）。

建议：将 Bash 版移入 `legacy/` 分支或归档，停止双维护。

### 修复 `prompt_yn` 的 stdin 读取

```rust
// main.rs:682 — read_to_string 读到 EOF，而非一行
stdin.read_to_string(&mut input).is_ok()
```

应改用 `BufRead::read_line`，否则 TTY 交互需 Ctrl-D 才能继续。

### 修复 tmpdir 泄漏

`install_pkgfile` 中临时目录仅在成功末尾清理，`?` 提前返回时泄漏。
应使用 RAII guard（类似 `LockGuard` 的 `Drop` 模式）。

## P2 - Medium

### 添加测试

当前状态：
- `pica-core`: 12 个单元测试（version/selector/manifest/repo）
- `pica-cli-rs`: 0 测试
- `pica-pack-rs`: 0 测试

优先覆盖：
- `install_pkgfile` happy path + error path（mock temp dirs）
- `find_pica_candidates_in_index` 筛选逻辑
- `upgrade_all` 版本比较与跳过逻辑
- `should_use_feeds` 各 policy 分支
- feed policy 决策矩阵
- `sync_repos` 的 repo.json 解析与索引合并

### 版本比较 fallback 不安全

```rust
// version.rs:37 — 非纯数字版本用字符串比较
} else {
    a >= b  // "9.0" >= "10.0" 返回 true
}
```

需要对混合版本字符串实现更健壮的比较（如 alphanumeric segment 拆分）。

### 错误码利用率低

90%+ 的错误构造都是 `CliError::new(DEFAULT_ERROR_CODE, ...)`。
应为关键失败场景定义更细粒度的错误码，提升 `--json-errors` 的机器可读性。

### 添加 CI/CD

- GitHub Actions: cargo build / test / clippy / fmt
- 交叉编译配置 (`.cargo/config.toml` 或 cross)
- 提交 `Cargo.lock`（binary crate 应锁定依赖）

## P3 - Low

### 文档修正

- `RUST_README.md` 引用了不存在的 `docs/architecture.md`
- 文档中 `md5` 字段但代码未实现，应标注或移除
- Bash 与 Rust 版本的行为差异未记录

### 补全空文件

- `installer.sh` — 空文件，应实现或移除
- `Makefile` — 空文件，应实现或移除

### `docs/standard.md` 格式问题

第 54-57 行 markdown 代码块未正确闭合（嵌套 ``` 语法错误）。
