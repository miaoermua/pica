# pica-rs 解耦记录：现阶段与第一阶段（2026-02-23）

本文档用于保留两类信息，便于后续排错与持续解耦：

- 现阶段结构快照（改造前认知）
- 第一阶段已落地改造（本次）

---

## 1. 现阶段快照（Before）

## 1.1 crate 边界

```text
pica-rs/
  crates/
    pica-core/
      error.rs
      io.rs
      manifest.rs
      repo.rs
      selector.rs
      version.rs
    pica-cli-rs/
      main.rs
    pica-pack-rs/
      main.rs
```

## 1.2 主要问题

- `pica-cli-rs` 主文件过大，流程与系统调用混杂；
- `pica-cli-rs` 与 `pica-pack-rs` 存在重复工具能力（临时目录、目录复制）；
- 共享“规则层”已在 `pica-core`，但“共享执行工具层”仍未充分下沉。

---

## 2. 第一阶段目标（Phase 1）

第一阶段只做**低风险共享下沉**，不改业务语义：

1. 把 `cli/pack` 重复的通用能力下沉到 `pica-core`；
2. `cli` 与 `pack` 改为复用 `core`；
3. 保持命令行为与外部接口不变。

---

## 3. 第一阶段已完成变更（本次）

## 3.1 `pica-core` 新增共享 IO 能力

文件：`crates/pica-core/src/io.rs`

- 新增 `now_unix_nanos()`
- 新增 `make_temp_dir(prefix)`
- 新增 `copy_dir_recursive(source, target)`
- 增加 Unix/非 Unix 的符号链接复制分支（`#[cfg(unix)]` / `#[cfg(not(unix))]`）

## 3.2 `pica-pack-rs` 复用 `pica-core::io`

文件：`crates/pica-pack-rs/src/main.rs`

- 引入 `make_temp_dir` 与 `copy_dir_recursive`
- 删除本地重复实现：
  - `create_temp_dir`
  - `copy_dir_recursive`
  - `copy_symlink`

## 3.3 `pica-cli-rs` 复用 `pica-core::io`

文件：`crates/pica-cli-rs/src/main.rs`

- `make_temp_dir()` 改为调用 `pica_core::io::make_temp_dir`
- `copy_dir_recursive()` 改为调用 `pica_core::io::copy_dir_recursive`
- 新增 `map_core_error()` 用于 `PicaError -> CliError`
- 清理未使用的本地 `now_unix_nanos()`

---

## 4. 验证记录

## 4.1 测试

执行命令：

```bash
cargo test --workspace
```

结果：通过（`pica-core` 12 个单测通过）。

## 4.2 二进制体积（release）

基线（重构前记录）：

- `pica-rs`: `571824` bytes

第一阶段后：

- `pica-rs`: `572680` bytes
- `pica-pack-rs`: `419000` bytes

说明：体积变化很小，处于正常优化波动范围。

---

## 5. 第二阶段建议（未实施）

第二阶段再做“功能模块解耦”，建议顺序：

1. `cli` 内先拆 `state`、`lock`、`query/remove`（低风险）
2. 再拆 `install/upgrade/sync`（高耦合主流程）
3. 尽量继续把纯逻辑下沉到 `pica-core`（planner/model）

---

## 7. 第二阶段（低风险拆分）已执行记录（2026-02-23）

说明：在第一阶段基础上，本次继续做了“低风险模块拆分”，不改命令语义。

### 7.1 已新增模块

- `crates/pica-cli-rs/src/lock.rs`
  - 抽离 `LockGuard` 与锁目录处理
- `crates/pica-cli-rs/src/commands/mod.rs`
  - 命令模块入口
- `crates/pica-cli-rs/src/commands/query.rs`
  - 抽离 `query_installed/query_info/query_license`
- `crates/pica-cli-rs/src/commands/remove.rs`
  - 抽离 `remove_pkg` 与 remove cmd 执行

### 7.2 主文件改动

- `crates/pica-cli-rs/src/main.rs`
  - 增加 `mod lock; mod commands;`
  - 改为从模块 `use` query/remove 的实现
  - 删除已迁移的内联函数定义

### 7.3 验证

执行：

```bash
cargo test --workspace
cargo build --workspace --release
```

结果：通过。

### 7.4 体积更新（release）

- 第二阶段后 `pica-rs`: `572888` bytes
- 第二阶段后 `pica-pack-rs`: `419000` bytes

相较第一阶段（`572680` bytes），`pica-rs` 变化极小，仍在正常波动范围。

---

## 8. 第二阶段（继续）已执行记录：sync/upgrade 拆分（2026-02-23）

在第二阶段低风险拆分基础上，继续完成：

- `crates/pica-cli-rs/src/commands/sync.rs`
  - 迁移 `sync_repos` 及其依赖检查辅助函数
- `crates/pica-cli-rs/src/commands/upgrade.rs`
  - 迁移 `upgrade_all`
- `crates/pica-cli-rs/src/commands/mod.rs`
  - 注册 `sync` 与 `upgrade` 模块
- `crates/pica-cli-rs/src/main.rs`
  - 改为使用模块导入
  - 删除已迁移的内联实现

验证：

```bash
cargo test --workspace
cargo build --workspace --release
```

结果：通过。

体积更新（release）：

- `pica-rs`: `572568` bytes
- `pica-pack-rs`: `419000` bytes

说明：相较上一轮 `572888` bytes，本轮体积略减。

---

## 9. 第三阶段进行中：install 主流程拆分 + 版本升级（2026-02-23）

### 9.1 install 模块拆分

新增：

- `crates/pica-cli-rs/src/commands/install.rs`

迁移了安装相关主函数：

- `install_app_auto`
- `install_app_via_opkg`
- `install_pica_from_repo`
- `install_pkg_source`
- `install_pkgfile`
- `sanitize_cache_filename`

并在 `main.rs` 中改为从 `commands::install` 导入调用。

### 9.2 全部 pica 版本升级到 `0.1.0`

已更新：

- `pica-rs/crates/pica-core/Cargo.toml`
- `pica-rs/crates/pica-cli-rs/Cargo.toml`
- `pica-rs/crates/pica-pack-rs/Cargo.toml`
- `pica-rs/crates/pica-core/src/lib.rs` (`PICA_VERSION`)
- `pica-cli/pica`
- `pica-pack/pica-pack`
- `pica-pack/example/hello/manifest`
- `docs/standard.md`
- `docs/developer.md`
- `pica-rs/docs/architecture.md`

> 说明：由于当前环境网络受限，`cargo update` 无法访问 crates.io；`Cargo.lock` 中 workspace 包版本已本地同步为 `0.1.0`。

### 9.3 验证

执行：

```bash
cargo test --workspace
cargo build --workspace --release
```

结果：通过。

### 9.4 体积（release）

- `pica-rs`: `574808` bytes
- `pica-pack-rs`: `419000` bytes


---

## 6. 排错建议

若第一阶段后出现异常，优先排查：

1. `copy_dir_recursive` 的符号链接行为是否与旧逻辑预期一致；
2. 临时目录路径生成是否影响已有调用链；
3. `map_core_error` 是否掩盖了特定错误码语义（当前统一映射为 `E_RUNTIME`）。
