# Pica 标准（草案）

本文档描述 pica 的“协议与规范”（偏标准），用于统一 `pica-pack` 与 `pica-cli` 的行为。

## 目标

- `pica-pack`：在构建机上把包打成 `pkg.tar.gz`（不使用 zst）
- `pica-cli`：在 OpenWrt 上提供 `pica` 命令，学习 Arch/pacman 的基本用法：
  - `-S` 同步（sync）
  - `-U` 安装/更新（install/update，类似 pacman -U）
  - `-R` 卸载（remove）

## 归档类型与载荷边界

- 当前唯一标准归档类型：`*.pkg.tar.gz`。
- `*.apk` 目前不作为可直接安装归档；后续可能引入兼容能力（以版本发布说明为准）。
- 在 OpenWrt 场景，`pkgmgr=opkg` 可结合 `src/` 分发非二进制资源（包括 `docker-compose` 相关文件）。
- Pica 仅负责包分发、校验、生命周期脚本执行，不提供 Docker 管理服务。

## 包格式（.pkg.tar.gz）

### 文件名

```
<pkgname>-<pkgver>-<pkgrel>-<platform>-<arch>.pkg.tar.gz
```

当 `platform = all` 时，为避免出现 `all-all`，文件名可简化为：

```
<pkgname>-<pkgver>-<pkgrel>-<arch>.pkg.tar.gz
```

说明：

- 文件名仍以 `platform` 作为 OpenWrt 目标代称。
- `arch` 同样保留在 `manifest` 中，用于区分 CPU/ABI 适配（例如 aarch64/a53）。

### 包内结构

压缩包根目录结构约定如下：

- `manifest`（必需，文件）
- `cmd/`（必需，目录）
- `binary/`（可选，目录）
- `depend/`（可选，目录；用于放 pica 封装的依赖 ipk）
- `src/`（可选，目录；用于放非 ipk 的资源/原始文件）

语义：

- `cmd/`：要安装到 `/usr/bin/` 的脚本/可执行文件。
- `binary/`：应用本体（或其 opkg 子包）对应的 `.ipk` 资源。
- `depend/`：可选；基础依赖的 `.ipk` 资源（允许只提供部分，交由 opkg 基于依赖信息补全）。
- `src/`：可选；生命周期脚本需要的源码/模板/静态资源，安装时复制到 `/usr/lib/pica/src/<pkgname>/`。

`binary/` 推荐结构（多变体）：

```
binary/<platform>/<arch>/*.ipk

`depend/` 推荐结构与 `binary/` 相同：

```
depend/<platform>/<arch>/*.ipk
```
```

打包器会把每个 `<platform>/<arch>` 组合单独生成一个 pica 包，并在产物中只保留对应组合的 `binary/` 与 `depend/`（若存在）；`src/`（若存在）会原样打入每个变体包。

## pica-pack 输出目录约定

默认情况下（不传 `--outdir`），`pica-pack` 会输出到 `pica-pack/bin/<pkgname>/`：

```
pica-pack/bin/<pkgname>/<pkgname>-<pkgver>-<pkgrel>-<platform>-<arch>.pkg.tar.gz
```

当 `platform = all` 时：

```
pica-pack/bin/<pkgname>/<pkgname>-<pkgver>-<pkgrel>-<arch>.pkg.tar.gz
```

## repo.json 强约束（schema + filename）

`pica -S` 同步阶段会对 `repo.json` 做严格校验：

- `schema` 必须为 `1`
- `packages` 必须为数组
- 每个 package 项必须包含非空字符串字段：
  - `pkgname`
  - `pkgver`
  - `pkgrel`
  - `platform`
  - `arch`
  - `filename`
  - `sha256`
- `sha256` 必须是 64 位十六进制字符串
- `filename` 必须是纯文件名（不能含 `/`、不能含 `..`），且必须以 `.pkg.tar.gz` 结尾
- `filename` 必须与其他字段一致：
  - `platform != all`：`<pkgname>-<pkgver>-<pkgrel>-<platform>-<arch>.pkg.tar.gz`
  - `platform = all`：`<pkgname>-<pkgver>-<pkgrel>-<arch>.pkg.tar.gz`

安装阶段（`-Si/-Sp`）在下载后、写入缓存和安装前，必须校验下载文件 SHA-256 与 `repo.json` 的 `sha256` 一致，否则失败。
- 可选 `download_url`（若提供）必须是 `http://`、`https://` 或 `file://`，用于覆盖默认下载路径

任一约束不满足，`pica -S` 会拒绝该 repo。

## URL 安装

`pica -U` 支持本地文件和 URL：

```
pica -U ./hello-0.2.1-1-all.pkg.tar.gz
pica -U https://example.invalid/pkgs/hello-0.2.1-1-all.pkg.tar.gz
```

允许的 URL 协议：

- `http://`
- `https://`
- `file://`

## JSON 输出（显式开启）

`pica` 默认输出面向人工的文本日志，不改变现有使用习惯。

可选参数：

- `--json`：启用 JSON 输出（成功/失败）
- `--json-errors`：仅失败时输出 JSON

## 非交互与来源策略

可选参数：

- `--non-interactive`：禁用交互提示，适配后端调用
- `--feed-policy <mode>`：来源策略
  - `ask`（默认）
  - `feed-first`
  - `packaged-first`
  - `feed-only`
  - `packaged-only`

## app 安装顺序

`app` 阶段固定分组顺序：

1. 主程序（core）
2. `luci-app-*`
3. `luci-i18n-*`

若某组为空则直接跳过（例如纯 CLI 包没有 luci/i18n）。

说明：

- 这些参数为显式开启；不传时行为保持不变。
- 当某些命令本身已经输出文本结果（例如 `-Q/-Qi/-Ql`），成功 JSON 会被自动抑制，避免混淆机器解析。
- `--json/--json-errors` 依赖 `jq`（缺失时会直接报错退出）。

## 兼容维度：os + arch + uname（platform 仅展示）

为了让安装/更新行为尽可能稳定，pica 的“实际兼容性判断”使用：

- `os`：系统类型（例如 `openwrt`），强约束

- `uname`：与 `uname -m` 严格匹配（跨系统最通用的基线）
- `arch`：OpenWrt/opkg 定义的架构字段（可通过 `opkg print-architecture` 查看），推荐统一使用 `all`

`platform` 仍然保留，但它只用于应用商店/仓库展示与筛选，不作为安装的硬性门槛。

在需要展示时（日志、查询）应同时展示 `os + platform + arch + uname`（若 `uname` 未提供则可省略）。

## manifest（Arch-like 文本）

### 格式

- `key = value` 一行一个字段
- 支持 `#` 注释
- 支持重复 key（表示数组）

### 必需字段

```
pkgname = <name>
pkgver = <version>
pkgrel = <pica-release>
platform = all
os = openwrt
arch = all
pica = <min pica-cli version>
```

### 最新推荐字段模板（0.2.1）

```ini
# Required
pkgname = hello
appname = hello
url = https://github.com/miaoermua/pica
luci_url = https://github.com/openwrt/luci/tree/master/applications/luci-app-hello
branch = stable
protocol = luci
luci_desc = LuCI plugin for hello service

pkgver = 0.2.1
pkgrel = 1
os = openwrt
platform = arm64
arch = all
pica = 0.2.1

# Optional metadata
pkgdesc = Example lifecycle package
packager = pica-pack
license = GPL-3.0-only
visibility = open

# Optional strong compatibility gate
# uname = aarch64

# Optional tags
# type = cli
# type = luci
# luci = lua1
#
# Optional source metadata
source = pica

# Optional package manager backend
# opkg: install/remove by app/base/kmod mapping (default)
# none: lifecycle-only mode, skip package-manager install/remove
pkgmgr = opkg

# Install plan
app = hello
app_i18n = luci-i18n-hello-{lang}
base = busybox
# kmod = kmod-tun

# Lifecycle scripts (optional)
cmd_install = cmd/install
cmd_update = cmd/update
cmd_remove = cmd/remove
```

关于 `pica` 字段：

- 表示“最低兼容 pica-cli 版本”（minimum required），不是“必须完全一致的版本”。
- 不写 `pica`：默认不做版本门槛校验（为了兼容旧包）。
- 写了 `pica` 且本机 `pica-cli` 版本低于该值：安装/更新会失败并提示无法兼容。

可选字段（强兼容校验，优先判断）：

```
uname = <uname>
```

### 可选字段（建议）

```
pkgdesc = ...
url = ...
luci_url = ...
packager = ...
builddate = <unix timestamp>
size = <bytes>

# license metadata
license = GPL-3.0-only
visibility = open
```

约定：

- `builddate`：可选；Unix 时间戳（秒）。推荐不要在“源码 manifest”里手写，由 `pica-pack build` 在构建产物中自动补全。
- `size`：可选；字节数（bytes）。推荐不要在“源码 manifest”里手写，由 `pica-pack build` 在构建产物中自动计算并补全。

校验文件（由 `pica-pack build` 自动生成，不写入 manifest）：

- `SHA256SUMS`：输出目录下的校验清单，格式为 `<sha256>  <filename>`
- 每次构建会为产物 `.pkg.tar.gz` 写入/更新对应条目

### 与 OpenWrt 安装相关的扩展字段（可重复，可选）

```
# Install plan (repeatable)
app = <opkg package name>
base = <opkg package name>
kmod = <opkg package name>

# Optional: i18n template for app packages
# The {lang} placeholder is resolved from pica config (default: zh-cn).
app_i18n = <opkg package name template>

# Program source metadata
url = <project homepage or repository URL>

# Optional LuCI source metadata
luci_url = <LuCI plugin homepage or repository URL>

# LuCI description metadata (optional)
luci_desc = <short LuCI plugin description>

# Package-manager backend (optional)
pkgmgr = <opkg|none>

# lifecycle cmd scripts (optional)
cmd_install = <relative file>
cmd_update = <relative file>
cmd_remove = <relative file>

# optional type tags (repeatable)
type = cli
type = luci

# when type includes luci
luci = lua1
```

约定：

- `app/base/kmod` 定义“安装清单”，pica 安装时必须遍历这些字段来决定需要安装的 opkg 包。
- `app_i18n` 允许包含 `{lang}` 占位符，pica 根据配置 `i18n` 选择实际 i18n 包名（当前仅 `zh-cn` 时参与安装/卸载）。
- `url` 用于记录程序来源（建议直接填仓库/项目地址，如 GitHub URL）。
- `luci_url` 用于记录 LuCI 插件来源（可选；仅 LuCI 包建议填写）。
- `luci_desc` 用于记录 LuCI 插件描述（与 `pkgdesc` 的通用程序描述分离）。
- `pkgmgr` 用于声明包管理后端：`opkg`（默认）或 `none`（仅生命周期脚本，不走包管理器安装/卸载）。
- 当 `pkgmgr = opkg` 时：按 `app/base/kmod` 安装、按 `app` + `app_i18n`（`i18n=zh-cn`）卸载。
- 当 `pkgmgr = none` 时：跳过包管理器安装/卸载，仅执行 `cmd_install/cmd_update/cmd_remove` 与 `cmd/` 文件部署。
- 若包内存在 `src/`：安装阶段复制到 `/usr/lib/pica/src/<pkgname>/`，卸载阶段清理该目录。
- `src/` 可用于携带脚本/模板/Compose 清单，但容器运行与编排策略由外部工具负责（不由 Pica 托管）。
- `cmd_install/cmd_update/cmd_remove` 是生命周期脚本（包内路径，一般在 `cmd/` 下），应按 POSIX `sh` 语法编写。
- 生命周期脚本由系统 `sh` 执行（OpenWrt 默认 BusyBox `sh`），不再依赖 `bash`。

- `type` 允许声明应用形态标签，便于 pica 在安装阶段做额外兼容检查。
- `type = luci` 表示“该包包含/依赖 LuCI Web UI”。如果声明了 `type = luci`，必须同时声明 `luci = lua1|js2`。
- `type = cli` 表示“该包提供纯命令行程序/脚本”。
- 仅声明 `type = cli` 时，不需要 `luci = ...`。
- `source` 用于记录安装来源（例如 `pica` / `opkg` / `local` / `url`），便于升级与排错。默认可不写；安装后 CLI 会在本地数据库里补全 `source`。

## arch（OpenWrt/opkg）

`arch` 不是 pica 自定义标签，它对应 OpenWrt/opkg 的架构字段。

推荐实践：

- 对绝大多数 pica 包：统一写 `arch = all`
- 仅当确实需要限制某些设备时：写 opkg 的具体 arch（例如 `aarch64_cortex-a53`）

`pica-cli` 的检查逻辑：

- `arch = all`：永远允许
- `arch != all`：必须出现在 `opkg print-architecture` 输出中，否则安装失败

## os（系统类型）

`os` 用于标识系统类型（例如 `openwrt`）。

`pica-cli` 的检查逻辑：

- `os = all`：永远允许
- `os != all`：必须与当前系统类型一致，否则安装失败

## OpenWrt：一个 app 多个 opkg 包

OpenWrt/LEDE 生态里，一个“应用”（你希望用一个 `pkgname` 表示）通常拆成多个 opkg 包：

```
myapp                 # 本体二进制/服务
luci-app-myapp        # LuCI 插件
luci-i18n-myapp-zh-cn # i18n
```

因此建议：

- `pkgname` 用“应用名”做 pica 包唯一标识。
- 安装清单用 `app = ...`（以及可选 `app_i18n = ...`）列出应用子包。
- 卸载时 pica 按 `app` + `app_i18n` 映射处理（当前仅 `i18n=zh-cn` 时处理 i18n 包）。

## cmd/.env 预留

`cmd/` 目录下允许存在一个可选文件：

```
cmd/.env
```

用途：为后续脚本/命令提供环境变量预留（默认不要求存在）。

建议约定：

- 安装时保存到 `/etc/pica/env.d/<pkgname>.env`
- 卸载时同步删除该 env 文件

## type/luci：LuCI 版本/实现（可选）

OpenWrt 上的 Web UI 可能存在不同实现（例如历史上的 Lua/LuCI1 与 luci2 JS）。

当你的包包含 LuCI 插件或强依赖某种 LuCI 实现时，建议：

- 增加 `type = luci`
- 声明 `luci` 的实现版本

```
type = luci
luci = lua1
```

或：

```
type = luci
luci = js2
```

约定：

- 未包含 `type = luci`：不做 LuCI 兼容检查（`luci = ...` 即使存在也不会触发检查）
- 包含 `type = luci`：必须同时声明 `luci = lua1|js2`
- `pica -U` 会尝试检测本机 LuCI 实现并匹配；无法检测或不匹配则安装失败

## 许可证（license / visibility）

manifest 中允许定义许可证信息：

```
license = GPL-3.0-only
visibility = open
```

约定：

- `license`：建议用 SPDX 标识（例如 `GPL-3.0-only`、`MIT`、`Apache-2.0`）
- `visibility`：软件可见性标签，取值只能是 `open|mix|closed`
- 当前版本只做“定义与展示”，不做任何许可证强制校验

## LICENSE 文件（包内可选）

打包时允许在 staging 根目录放一个 `LICENSE` 文件，`pica-pack` 会把它原样打进压缩包根目录：

```
LICENSE
```

当前版本不自动安装/展开该文件，仅作为后续 `pica` 命令显示许可证内容的基础。

## app 选择器

`pica -Si/-Sp` 支持以下选择器（全角符号也支持）：

```
app
app(branch)
app:branch
```

约定：

- 当前选择器仅支持按 `branch` 过滤。
- 当前不提供历史版本安装，仓库仅保留最新包。

## manifest 示例（LuCI1）

```
pkgname = luci-app-example
appname = example
url = https://github.com/example/example
luci_url = https://github.com/openwrt/luci/tree/master/applications/luci-app-example
branch = openwrt-23
protocol = luci
luci_desc = Example LuCI plugin for OpenWrt

pkgver = 1.0.0
pkgrel = 1
pkgmgr = opkg

pkgdesc = Example LuCI application
packager = example
license = GPL-3.0-only
visibility = open

arch = all
os = openwrt
platform = amd64
uname = x86_64

pica = 0.2.1
source = pica

type = luci
luci = lua1

# Install plan (repeatable)
app = luci-app-example
app_i18n = luci-i18n-example-{lang}
base = luci-base
base = rpcd
base = ca-bundle
kmod = kmod-tun

# Lifecycle scripts (optional)
cmd_install = cmd/install
cmd_update = cmd/update
cmd_remove = cmd/remove
```
