# Pica 开发者文档（WIP）

本仓库当前包含两个子项目：

- `crates/pica-pack-rs/`：核心 Rust 打包器（在构建机上生成 `.pkg.tar.gz`）
- `crates/pica-cli-rs/`：核心 Rust OpenWrt 安装/管理器（提供 `pica-rs` 命令）

目标是“学习 Arch/pacman 的使用方式”，但在 OpenWrt 环境里以 `opkg` 完成真正的安装/卸载动作，类似于滚动更新。

## 目录结构

```
.
  crates/
    pica-core/
    pica-cli-rs/
    pica-pack-rs/
  legacy/pica-pack/
    pica-pack
    example/
      hello/
        manifest
        binary/
        cmd/
  legacy/pica-cli/
    pica
  docs/
    standard.md
    developer.md
```

## 版本约定

- `pica-cli` 内置协议版本：`PICA_VERSION=0.2.6`
- `manifest` 的 `pica` 字段表示最低兼容版本：`pica = <min pica-cli version>`（可选，不写不检查）
- `pica -U` 安装时会校验 `manifest` 的 `pica` 与 CLI 是否一致；不一致直接失败（非 0 退出）。

## pica 包格式（.pkg.tar.gz）

### 文件名

```
<pkgname>-<pkgver>-<pkgrel>-<platform>-<arch>.pkg.tar.gz
```

当 `platform = all` 时，为避免 `all-all`，文件名简化为：

```
<pkgname>-<pkgver>-<pkgrel>-<arch>.pkg.tar.gz
```

说明：

- `platform`：仅用于应用商店/仓库展示；本项目默认统一写 `all`
- `uname`：优先用于兼容性判断；建议用 pica 口径：
  - `amd64`（兼容 `x86_64`）
  - `arm64`（兼容 `aarch64`）
- `arch`：OpenWrt/opkg 定义的架构字段（推荐统一用 `all`）
- 文件名仍以 `platform` 为主（更贴近 OpenWrt target 发布）。
- `arch` 写入 `manifest`，用于安装时校验与展示。
- `pkgver` 推荐形如 `1.2.3`（语义版本）
- `pkgrel` 用于 pica 打包修订号（滚动更新）

### 包内结构

压缩包根目录约定：

```
manifest
cmd/
binary/   (optional)
src/      (optional)
```

- `manifest`：包元数据（Arch-like `key = value` 文本）
- `cmd/`：必需，安装到 `/usr/bin/` 的脚本/可执行文件
- `binary/`：可选，通常放 `.ipk` 或其他二进制资源；纯脚本包可以没有该目录
- `src/`：可选，放非 ipk 的源文件/脚本资源；安装时会复制到 `/usr/lib/pica/src/<pkgname>/`

`binary/` 推荐布局（多变体 ipk）：

```
binary/<platform>/<arch>/*.ipk
```

## pica-pack 输出约定

默认输出目录（不传 `--outdir`）：

```
legacy/pica-pack/bin/<pkgname>/
  <pkgname>-<pkgver>-<pkgrel>-<platform>-<arch>.pkg.tar.gz
```

当 `platform = all` 时：

```
legacy/pica-pack/bin/<pkgname>/
  <pkgname>-<pkgver>-<pkgrel>-<arch>.pkg.tar.gz
```

## manifest（Arch-like 文本）

### 格式规则

- 一行一个字段：`key = value`
- 支持 `#` 注释
- 支持重复 key（在 CLI 中会变成数组）

### 必需字段

```
pkgname = <name>
pkgver = <version>
pkgrel = <pica-release>
platform = all
os = openwrt
pica = <min pica-cli version>
arch = all
```

可选字段：

```
uname = <uname -m>
```

应用选择器（`-S <selector>`/`-Si`/`-Sp`）：

```
app
app(branch)
app:branch
```

说明：

- 当前仅支持按 `branch` 过滤。
- 当前仓库仍是滚动更新，不保留历史可安装包。

### OpenWrt 扩展字段

```
app = <opkg-package>
base = <opkg-package>
kmod = <opkg-package>

# logical app identity (required)
appname = <logical app name>
os = <openwrt|linux|...>
platform = <display label, e.g. arm64/amd64>
url = <project homepage or repository URL>
luci_url = <LuCI plugin homepage or repository URL>
branch = <distribution branch>
protocol = <luci|cli|...>
luci_desc = <short LuCI plugin description>
pkgmgr = <opkg|none>

# optional
app_i18n = luci-i18n-foo-{lang}

# optional compatibility tag
# luci = lua1
# luci = js2

# optional type tags (repeatable)
# type = cli
# type = luci

# license metadata
# license = GPL-3.0-only
# visibility = open
```

### arch（OpenWrt/opkg）

`arch` 值来自 OpenWrt/opkg（可通过 `opkg print-architecture` 查看）。

推荐：

```
arch = all
```

当确实需要限制设备时，可以写某个具体的 opkg arch（例如 `aarch64_cortex-a53`）。

语义：

- `app/base/kmod`：安装清单（重复字段），决定需要安装哪些 opkg 包。
- `app_i18n`：i18n 包名模板（`{lang}` 从配置 `i18n` 读取，默认 `zh-cn`；当前仅 `zh-cn` 时参与安装/卸载）。
- `url`：程序来源（建议填写仓库/项目 URL）。
- `luci_url`：LuCI 插件来源（可选；仅 LuCI 包建议填写）。
- `luci_desc`：LuCI 插件描述（区别于通用 `pkgdesc`）。
- `pkgmgr`：包管理后端，`opkg`（默认）或 `none`（仅生命周期脚本）。
- `pkgmgr=opkg` 时按 `app` + `app_i18n` 映射执行卸载；`pkgmgr=none` 时跳过包管理器卸载。
- `src/`：可承载未编译资源（脚本、模板、Compose 清单等），由生命周期脚本决定部署位置与方式。
- 边界：Pica 不负责 Docker 管理，不提供 `dockerd`/`docker compose` 的托管与状态编排。

### OpenWrt 的“一个 app 多个 opkg 子包”

OpenWrt 生态里，一个“应用”常常拆分为多个 opkg 包，例如：

```
myapp
luci-app-myapp
luci-i18n-myapp-zh-cn
```

推荐在同一个 pica 包的 `manifest` 中用 `app = ...`（以及可选 `app_i18n = ...`）明确列出子包，这样 `pica -R myapp` 仅按该映射卸载。

## pica-pack（打包器）

### 脚本

- 入口：`legacy/pica-pack/pica-pack`

### 用法

```
./legacy/pica-pack/pica-pack build <staging_dir> [--outdir DIR]
```

其中 `staging_dir` 需要包含：

```
<staging_dir>/manifest
<staging_dir>/cmd/

  # optional
  <staging_dir>/binary/
  <staging_dir>/src/
```

输出日志风格参考 Arch `makepkg`：

```
==> Making package: hello 0.2.6-1 (openwrt-any)
  -> Pica version: 0.2.6
  -> Creating archive...
==> Finished: /tmp/pica-test/hello-0.2.6-1-openwrt-any.pkg.tar.gz
```

### 示例

- 示例包 staging：`legacy/pica-pack/example/hello/`
- 构建：

```
./legacy/pica-pack/pica-pack build legacy/pica-pack/example/hello --outdir /tmp/pica-test
```

## pica-cli（OpenWrt 命令行）

### 依赖

- `sh`（BusyBox 默认 shell）
- `jq`
- `tar`
- 下载工具其一：`uclient-fetch`（OpenWrt 常见）/ `wget` / `curl`
- `opkg`（仅 `-U/-R` 需要）

### 安装位置与文件

- 配置目录：`/etc/pica/`
- 配置文件：`/etc/pica/pica.json`
- 环境变量目录（预留）：`/etc/pica/env.d/`
- 状态目录：`/var/lib/pica/`
  - 安装数据库：`/var/lib/pica/db.json`
  - 仓库索引：`/var/lib/pica/index.json`
  - repo 缓存：`/var/lib/pica/cache/repos/<name>.json`
  - 并发锁文件：`/var/lib/pica/db.lck`

并发约束：

- `pica` 运行时会持有全局锁，避免多个安装/同步事务并发写状态文件。
- 当前使用目录锁（`db.lck.d`）避免并发事务。
- 若检测到锁目录存在但持有 PID 已退出，会自动清理僵尸锁并重试加锁。
- `opkg update` 的锁冲突恢复也采用 PID 检测：仅当 `opkg.lock` 持有 PID 不存在时才会清理并重试。

开发/测试时可用环境变量覆盖路径（避免写入系统目录）：

```
PICA_ETC_DIR=/tmp/pica-etc PICA_STATE_DIR=/tmp/pica-var pica -S
```

### 命令

可选机器可读输出参数（显式开启）：

- `--json`：命令成功/失败都尝试输出 JSON（当命令已输出文本结果时，为避免混流，成功 JSON 会自动抑制）
- `--json-errors`：仅在失败时输出 JSON 错误对象

可选非交互参数（后端/自动化推荐）：

- `--non-interactive`：禁用交互提示
- `--feed-policy <mode>`：安装来源策略
  - `ask`（默认）
  - `feed-first`
  - `packaged-first`
  - `feed-only`
  - `packaged-only`

应用安装顺序（app 阶段固定）：

- `core`（主程序，非 `luci-app-*` 且非 `luci-i18n-*`）
- `luci-app-*`
- `luci-i18n-*`

说明：纯 CLI 应用只有 `core` 组时，后两组会自动跳过。

#### 同步（-S）

```
pica -S
:: Synchronizing package databases...
:: main downloading...
:: main updated
```

行为：

- 读取 `/etc/pica/pica.json` 的 `repos[]`
- 对每个 repo 下载 `<url>/repo.json`
- 写入/更新 `/var/lib/pica/index.json`

#### 安装（-S <selector>）

```
pica -S myapp
```

行为：

- 当 `-S` 带 selector 参数时，直接进入安装流程（auto 模式）
- 自动在 `opkg` 与 `pica` 仓库安装路径之间决策

#### 远端查询（-Si <selector>）

```
pica -Si myapp
```

行为：

- 从已同步的 `index.json` 中筛选 selector 命中的候选包
- 按版本选择最新候选并展示仓库远端元信息（不安装）

错误规则：

- 未配置软件源（`repos` 为空）会直接失败（非 0 退出）

#### 安装/更新（-U）

```
pica -U ./hello-0.2.6-1-openwrt-any.pkg.tar.gz

#### 全量升级（-Syu）

```
pica -Syu
```
```

行为：

- 解包并校验：必须包含 `manifest/cmd`；`binary/depend/src` 为可选
  - 校验 `pica` 协议版本一致
  - `platform`：仅展示，不作为安装门槛
  - 校验 `uname`（若提供）：优先匹配（`amd64/arm64` 与 `x86_64/aarch64` 做别名兼容）
  - 校验 `arch`：OpenWrt/opkg 架构字段，推荐 `arch = all`；若不是 all，则必须出现在 `opkg print-architecture`
- 读取 `manifest` 的安装清单字段：`kmod/base/app`（以及可选的 `app_i18n`）。
- 先处理 `kmod`：缺失/不可安装则拒绝继续。
- 在执行 `opkg install` 前会先检查索引缓存（`/var/opkg-lists/`，OpenWrt 常见映射为 `/tmp/opkg-lists/`）：仅当缓存缺失时才执行 `opkg update`，避免每次重复更新。
- 若 `opkg update` 因锁冲突失败（`/var/lock/opkg.lock`），会尝试清理锁文件并自动重试一次更新。
- 再处理 `base`：软件源有则可选择走软件源；软件源没有则必须使用包内 `depend/*.ipk`，否则失败。
- 最后处理 `app`：软件源有则可选择走软件源；软件源没有则必须使用包内 `binary/*.ipk`，否则失败。
- 安装命令：将 `cmd/` 复制到 `/usr/bin/`
- 若包内存在 `src/`：复制到 `/usr/lib/pica/src/<pkgname>/`，供生命周期脚本读取
- 写入本地安装数据库：`/var/lib/pica/db.json`
- 写入安装审计报告：`/var/lib/pica/install-report.json`
  - 安装前依赖可用性检查（kmod/base/app）
  - 整个事务新增依赖列表
  - app 阶段新增依赖列表

#### 卸载（-R）

```
pica -R myapp
:: Removing myapp...
:: removing opkg package: myapp
:: removing opkg package: luci-app-myapp
:: Transaction completed
```

行为：

- 只按 `manifest` 的 `app = ...` 和 `app_i18n = ...` 计算卸载列表（`i18n=zh-cn` 时包含语言包）
- 不做依赖保护/依赖树分析（依赖策略交给 opkg 与用户）

#### 查询（-Q）

```
pica -Q
hello	0.2.6-1	amd64
```

## 仓库协议（repo.json，最小实现）

### 服务器目录结构（建议）

```
repo-root/
  repo.json
  packages/
<pkgname>-<pkgver>-<pkgrel>-<platform>-<arch>.pkg.tar.gz
```

### /etc/pica/pica.json（建议）

```
{
  "repos": [
    {
      "name": "main",
      "url": "https://example.invalid/pica",
      "platform": "openwrt-any"
    }
  ]
}
```

### repo.json（建议）

```
{
  "schema": 1,
  "name": "main",
  "updated_at": 1700000000,
  "packages": [
    {
      "pkgname": "hello",
      "pkgver": "0.2.6",
      "pkgrel": "1",
      "appname": "hello",
      "url": "https://github.com/miaoermua/pica",
      "luci_url": "https://github.com/openwrt/luci/tree/master/applications/luci-app-hello",
      "pkgmgr": "opkg",
      "branch": "stable",
      "os": "openwrt",
      "platform": "amd64",
      "pica": "0.2.6",
      "filename": "hello-0.2.6-1-amd64-all.pkg.tar.gz",
      "sha256": "<sha256>",
      "size": 465
    }
  ]
}
```

当前 `pica -S` 对 `repo.json` 启用严格校验（强约束）：

- `schema` 必须是 `1`
- `packages` 必须是数组
- 每个包条目必须包含非空字符串字段：`pkgname/pkgver/pkgrel/platform/arch/filename/sha256`
- `os` 推荐始终声明（建议 `openwrt`）
- `sha256` 必须是 64 位十六进制字符串
- `filename` 必须是纯文件名（不能含 `/`、不能含 `..`），并且必须以 `.pkg.tar.gz` 结尾
- `filename` 必须与字段一致：
  - `platform != all`：`<pkgname>-<pkgver>-<pkgrel>-<platform>-<arch>.pkg.tar.gz`
  - `platform = all`：`<pkgname>-<pkgver>-<pkgrel>-<arch>.pkg.tar.gz`
- 可选 `download_url`（若提供）必须是 `http://`、`https://` 或 `file://`；安装时优先使用该 URL 下载

当前 `pica -S` 语义为：无参数时同步；带 selector 时安装。

当前 `pica -S <selector>/-Sp` 从仓库下载 `.pkg.tar.gz` 时，会在写入缓存和安装前执行 SHA-256 校验；若不匹配会直接失败。
