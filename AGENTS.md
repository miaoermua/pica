# AGENTS.md

Pica（Pica Is a Compact Archiver）是面向 OpenWrt 的紧凑包管理器，Rust workspace，产出两个二进制：

- `pica`（`crates/pica-pkg-cli`）：设备端 CLI，负责同步、安装、升级、卸载、查询
- `pica-pack`（`crates/pica-pkg-pack`）：构建端打包器，把 `pkgdir` 打成 `.pkg.tar.gz` 并生成入口 `index.json`、后端 `index.json` 与 `SHA256SUMS`

协议（包格式、文件名、`manifest`、`index.json`、`db.json`）的权威文档是 `docs/standard.md`，内部机制说明在 `docs/developer.md`，实现在做/不做的清单在 `crates/README.md`。改代码前先读这三份中相关的一节，不要凭猜测改协议。

## 命令

```bash
cargo build --workspace --release          # 构建
cargo test --workspace --locked            # 全量测试（CI 用 -- --nocapture）
cargo test -p pica-pkg-cli --test db_semantics      # 单个集成测试文件
cargo fmt --all                            # 格式化（rustfmt.toml：max_width 100，tab_spaces 2）
cargo clippy --workspace --all-targets     # lint（clippy.toml：msrv 1.85，too-many-lines-threshold 100）
dprint fmt                                 # 仅格式化 **/*.md（dprint.json，lineWidth 100）

cargo run -p pica-pkg-pack -- build <pkgdir> [--outdir DIR] [--pkgmgr opkg|apk|pica|opkg,apk]
cargo run -p pica-pkg-cli -- -Q
```

- CI（`.github/workflows/openwrt-rust-binaries.yml`）跑 `cargo fmt --check`、`cargo clippy -- -D warnings`、`dprint check`、`cargo test` 和 `cross build`（`x86_64-unknown-linux-musl` / `aarch64-unknown-linux-musl` / `mipsel-unknown-linux-musl`；mipsel 是 tier 3，用 nightly + `-Zbuild-std=std,panic_abort`，因此代码里不能用 `AtomicU64` 这类 32 位 MIPS 没有的类型）。
- `Makefile` 与 `installer.sh` 当前是空文件，没有可用目标。
- workspace 级 lints：`unsafe_code = "forbid"`，clippy `all` + `pedantic` 均为 `warn`；三个 crate 都 `[lints] workspace = true`，pedantic 告警要处理而不是加 `#[allow]` 绕过。

## 代码地图

```text
crates/pica-pkg-core/src/     协议与共享类型：manifest.rs / repo.rs / layout.rs / naming.rs / version.rs
                              / selector.rs / constraint.rs / io.rs / error.rs（无 CLI 依赖）
crates/pica-pkg-cli/src/      main.rs（手写参数解析，不用 clap）、app.rs（路径与环境）、i18n.rs
    commands/                 install/{mod,pipeline,precheck,optional,feed}.rs、sync.rs、query.rs
                              、remove.rs、setup.rs、upgrade.rs
    pkgmgr/                   opkg.rs、apk.rs、apk_repositories.rs、index.rs（依赖索引探测）
    candidate.rs              候选筛选（appname / branch / os / backend）
    source.rs                 源地址拼接与 FetchPolicy（scheme/主机/重定向判定）
    platform.rs、state.rs、installed.rs、lock.rs、system.rs、format.rs、constraints.rs、menu.rs
crates/pica-pkg-cli/tests/    集成测试：db_semantics.rs、setup.rs、source_indexes.rs（assert_cmd 驱动 `pica`）
crates/pica-pkg-pack/src/     build.rs、matrix.rs、backend.rs、payload.rs、archive.rs、container.rs
                              、rewrite.rs（构建期 manifest 重写）
```

## 协议冻结：改这些等于改外部契约

外部消费者是软件源仓库（`pica-pkg-repo` 的 CI 与已发布索引）和已装机的设备。下列任何一项变化都会让已发布的索引/产物失配，动手前必须先与维护者确认，并优先找向后兼容的最小方案：

- 产物文件名：`<pkgname>-<pkgver>-<pkgrel>-<platform>-<arch>[-opkg|-apk|-pica].pkg.tar.gz`，`platform = all` 时省略 platform 段；后缀只由 `pkgmgr` 单向生成，永不反向解析
- 包内结构：`manifest`（必需）+ `scripts/` + `root/` + `pkgs/`（仅 opkg/apk）+ `LICENSE`
- 源根 `index.json` 与后端 `index.json`：`schema` 必须为 `1`，必需字段与 `filename` 一致性校验（见 `docs/standard.md`「后端索引强约束」）；未识别字段要原样保留（flatten）
- `db.json` 字段语义：`source`（渠道）与 `pkgmgr`（实际后端）职责不同，旧记录的解析阶梯不能变
- 状态路径：`/etc/pica/pica.json`、`/etc/pica/env.d/`、`/var/lib/pica/{db.json,index.json,db.lck,cache/}`、`/usr/lib/pica/scripts/<pkgname>/`
- `manifest` 字段名、`<key>@<backend>` 覆盖规则、`app_i18n` 的 `{lang}` 模板
- 命名规则：`appname` 是选择键（纯上游应用名，不含 `@`），`pkgname` 是源内身份（`-bin` / `@<谁>` 只进 `pkgname`）；字符集由 `pica_pkg_core::naming` 判定

行为级修复同样要 bump 版本：改根 `Cargo.toml` 的 `[workspace.package] version`（当前 `0.4.5`），`PICA_VERSION` 随之变化。

## 约定

- **依赖最小化**（OpenWrt 体积敏感）：core/cli 只依赖 `serde`、`serde_json`、`sha2`、`tempfile`、`thiserror`；参数解析手写；不要引入 `clap`/`reqwest`/`tokio` 之类重依赖。系统能力交给系统工具：`opkg`/`apk`、`tar`、下载器 `curl` → `wget` → `uclient-fetch`。
- **不做的事**（`crates/README.md` Non-goals）：不写回 `opkg`/`apk` 的已安装数据库、不做依赖求解、不解析容器格式（唯一例外是 `pica-pack` 为统计 `size` 读 `.ipk`/legacy `.apk`）。
- **所有用户可见文本走 `src/i18n.rs`**：新增文案要在 `Key` 加变体、在 `english()` / `chinese()` 两张穷尽 `match` 各补一行、并加进 `ALL_KEYS`（有测试校验两表 `{n}` 占位符个数一致）。业务代码里不写语言判断、不写裸英文句子（没有 `App` 时用 `i18n::tr`；测试 `no_sentence_is_written_outside_this_table` 会拦住裸句子）。包名、命令、配置键、协议字段、路径、`E_*` 错误码、JSON 输出以及 `--version`/`-Qi` 的字段名不翻译；第三方工具的原始错误文本作为参数塞进占位符，不能丢。
- **输出诚实**：只打印真实发生的阶段和可信的数字。`size`/`csize` 缺失或为 `0` 就不报那一行（`0` 表示源没测量，不是不占空间）；统计不出来的容器（apk-tools 3.x 的 ADB）告警为未计量，不能记 0；实际写入量由安装结束时的 `Installed <n> files, <bytes>` 给出，两者不互相替代。
- **错误显式**：用 `E_*` 错误码（`E_MANIFEST_INVALID`、`E_INDEX_UNAVAILABLE`、`E_INTEGRITY_INVALID` 等），不要用隐式兜底掩盖问题。读不到依赖索引是 `E_INDEX_UNAVAILABLE`，绝不能当成"源里没有这个包"。
- **路径安全**：`root/` 内必须是相对路径、不含 `..`、不以 `/` 开头；符号链接原样保留且目标不得逃逸 `root/`。判定都在 `pica_pkg_core::io`：构建期用 `symlink_escapes_root`，安装期用 `install_payload` / `resolve_under`（错误文本 `escapes the target root`），两侧语义不能分叉。
- **状态写入是原子的**：`io::write_json_file_pretty`（`state.rs` 用它写 `db.json` / `index.json`）内部就是 `io::write_atomic`（临时文件 + rename），下载缓存也走 `write_atomic`；不要改用直接覆盖写。
- **兼容性判定**用 `os` + `arch` + `uname`；`platform` 只用于展示与筛选，不作为安装硬门槛。
- **测试**：单元测试内联 `#[cfg(test)]`（用 `pretty_assertions`），端到端行为用 `crates/pica-pkg-cli/tests/` 里的集成测试（`assert_cmd` + `predicates` 驱动真实二进制）。改了行为就补测试。

## 本地调试

用环境变量把状态目录挪出系统路径，避免写坏本机根目录：

```bash
PICA_ETC_DIR=/tmp/pica-etc PICA_STATE_DIR=/tmp/pica-var pica -S
```

覆盖点集中在 `crates/pica-pkg-cli/src/app.rs` 的 `Paths::from_env`，除目录外还能逐个文件覆盖：`PICA_CONF_FILE`、`PICA_DB_FILE`、`PICA_INDEX_FILE`、`PICA_CACHE_DIR`、`PICA_REPOS_CACHE_DIR`、`PICA_REPORT_FILE`、`PICA_LOCK_FILE`。调试后端调用可加 `--non-interactive`、`--feed-policy <mode>`、`--json` / `--json-errors`（`--json-errors` 的 `code` 恒定，只有 `message` 随语言变）。

## 文档

文档只描述代码里真实存在的行为：改了行为就同步改 `docs/standard.md`（协议）/ `docs/developer.md`（机制），不要把未实现的计划写进去。`README.md` 落后于实现时只修落后的描述，不顺手重写。

## 协作

- 不要自行 `git commit` / `git push` / 建 PR / 发版本，除非明确要求；跨仓库改动由维护者本人提交。
- 仓库根目录的 `state/`、`.codebuddy/`、`.workbuddy/`、`docs/local_agent_cache/` 都在 `.gitignore` 里，是本地运行/缓存产物，不要提交也不要当成源码。
