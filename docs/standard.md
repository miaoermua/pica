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
- `pkgs/` 里可以放后端原生包（`.ipk` / `.apk`），它们不是 Pica 归档，只是随包分发给后端安装的载荷。
- `pkgmgr` 是**包身份**的一部分：`opkg`、`apk`、`none` 三种后端各自独立打包、独立索引、独立安装。
- 只有 `opkg` / `apk` 会调用系统包管理器；`none` 由 Pica 自己部署 `root/`，不调用任何包管理器。
- Pica 仅负责包分发、校验、生命周期脚本执行，不提供 Docker 管理服务。

## 包格式（.pkg.tar.gz）

### 文件名

```
<pkgname>-<pkgver>-<pkgrel>-<platform>-<arch><suffix>.pkg.tar.gz
```

当 `platform = all` 时，为避免出现 `all-all`，文件名简化为：

```
<pkgname>-<pkgver>-<pkgrel>-<arch><suffix>.pkg.tar.gz
```

`<suffix>` 只由 `pkgmgr` 单向生成，永不反向解析：

| `pkgmgr` | `<suffix>` |
| --- | --- |
| `opkg` | （空） |
| `apk` | `-apk` |
| `none` | `-none` |

说明：

- 文件名仍以 `platform` 作为 OpenWrt 目标代称。
- `arch` 同样保留在 `manifest` 中，用于区分 CPU/ABI 适配（例如 aarch64/a53）。

### 包内结构

压缩包根目录结构约定如下：

- `manifest`（必需，文件）
- `scripts/`（可选，目录）
- `root/`（可选，目录）
- `pkgs/`（可选，目录；仅 `opkg` / `apk` 后端会包含）
- `LICENSE`（可选，文件）

语义：

- `scripts/`：生命周期脚本，**不进 PATH**。安装时复制到 `/usr/lib/pica/scripts/<pkgname>/`，`scripts/.env` 复制到 `/etc/pica/env.d/<pkgname>.env`。
- `root/`：按目标路径镜像的载荷。`root/etc/init.d/demo` → `/etc/init.d/demo`，`root/usr/bin/demo` → `/usr/bin/demo`。
- `pkgs/`：要交给系统包管理器安装的原生包，打包时按后端过滤后拍平存放（见下）。

`scripts/` 中约定的文件名与 `manifest` 的对应关系：

| 文件 | `manifest` 字段 | 执行时机 |
| --- | --- | --- |
| `scripts/install` | `script_install` | 首次安装时执行 |
| `scripts/update` | `script_update` | 覆盖安装/升级时执行 |
| `scripts/remove` | `script_remove` | 卸载时执行 |
| `scripts/.env` | — | 提供默认 `PICA_KEEP_SCRIPT_*` 策略 |

`root/` 内的路径必须是相对路径，且不得包含 `..`、不得以 `/` 开头；安装时若发现逃逸路径会直接拒绝该包。

`pkgs/` 的源目录结构按后端和变体分：

```
pkgs/<backend>/<platform>/<arch>/*.<ipk|apk>
pkgs/<backend>/*.<ipk|apk>          # 无变体时的简化写法
```

打包器会把每个 `<platform>/<arch>` 组合单独生成一个 pica 包，并只保留该组合的 `pkgs/<backend>/` 内容；`scripts/` 与 `root/` 会原样打入每个变体包。`none` 后端不读取 `pkgs/`。

## pica-pack 输出目录约定

默认情况下（不传 `--outdir`），`pica-pack` 输出到 `pica-pack/bin/<pkgname>/<backend>/`：

```
pica-pack/bin/<pkgname>/opkg/<pkgname>-<pkgver>-<pkgrel>-<arch>.pkg.tar.gz
pica-pack/bin/<pkgname>/apk/<pkgname>-<pkgver>-<pkgrel>-<arch>-apk.pkg.tar.gz
```

每个后端都有自己的子目录。后端由目录表达，而不是从扩展名推断。

## repo.json 强约束（schema + filename）

`pica -S` 同步阶段会对 `repo.json` 做严格校验：

- `schema` 必须为 `2`
- `packages` 必须为数组
- 每个 package 项必须包含非空字符串字段：
  - `pkgname`
  - `pkgver`
  - `pkgrel`
  - `pkgmgr`（`opkg` / `apk` / `none`）
  - `os`
  - `platform`
  - `arch`
  - `filename`
  - `sha256`
- `size` 必须是数字，表示 `root/` 载荷解压后的总字节数（`pica-pack` 构建时统计）
- `csize` 必须是数字，表示归档文件的压缩后字节数
- `sha256` 必须是 64 位十六进制字符串
- `filename` 必须是纯文件名（不能含 `/`、不能含 `..`），且必须以 `.pkg.tar.gz` 结尾
- `filename` 必须与其他字段一致：
  - `platform != all`：`<pkgname>-<pkgver>-<pkgrel>-<platform>-<arch><suffix>.pkg.tar.gz`
  - `platform = all`：`<pkgname>-<pkgver>-<pkgrel>-<arch><suffix>.pkg.tar.gz`
  - `<suffix>` 见上文文件名表
- 未识别的额外字段会被原样保留（`flatten`），不会在同步时丢失

安装阶段（`-S <selector>`/`-Sp`）在下载后、写入缓存和安装前，必须校验下载文件 SHA-256 与 `repo.json` 的 `sha256` 一致，否则失败。
- 可选 `download_url`（若提供）必须是 `http://`、`https://` 或 `file://`，用于覆盖默认下载路径

同后端同名包在同一索引中只允许出现一次；`opkg`、`apk`、`none` 三种后端的同名包可以并存，因为 `pkgmgr` 参与包身份。

任一约束不满足，`pica -S` 会拒绝该 repo。

## URL 安装

`pica -U` 支持本地文件和 URL：

```
pica -U ./hello-0.2.8-1-all.pkg.tar.gz
pica -U https://example.invalid/pkgs/hello-0.2.8-1-all.pkg.tar.gz
```

允许的 URL 协议：

- `http://`
- `https://`
- `file://`

## 安装数据库（db.json）字段语义

`/var/lib/pica/db.json`（可用 `PICA_STATE_DIR` 重定向）记录已安装项。每个条目的字段回答不同的问题，不能互相替代：

- `source`：**这个包是怎么被 Pica 拿到的**，取值 `pica` / `local` / `url` / `feed`。
- `pkgmgr`：**哪个后端真正拥有这些文件**，取值 `opkg` / `apk` / `none`。
- `pkgfile`：产物安装时是归档路径；`feed` 安装时是后端名（因为不存在 Pica 归档）。

写入规则：

- `source` 与 `pkgmgr` 在安装时**覆盖写入**，不沿用产物 manifest 里的值：
  - `--pkgmgr none -U x.pkg.tar.gz` 会记住 `pkgmgr = none`，即使归档声明的是 `apk`，卸载时也就不会去驱动 apk。
  - URL 下载会记住 `source = url`，即使打包时写的是 `pica`。
- 因此 `source` 描述渠道，`pkgmgr` 描述后端；二者取值空间不重叠（历史遗留的 `source = opkg` 除外，见下）。

读取规则（旧记录迁移，无需改写数据库）：

- 后端解析阶梯：`pkgmgr` → 旧 `source` 中的后端名 → 旧 `pkgfile` 中的后端名 → `pkgfile` 是归档路径时取 `none` → 兜底 `opkg`。
- 渠道解析：`source` 为可识别渠道时直接用；否则按 `pkgfile` 是否为归档路径判定 `pica` 或 `feed`。
- 第 2~5 档属于推断，`pica -R` 会打出 warning 说明后端来源；`-Qi` 展示的是归一化后的渠道与后端。

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
- `--json/--json-errors` 由 Rust 侧直接输出 JSON，不依赖任何外部 JSON 工具。

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
- 支持后端覆盖 key：`<key>@<backend>`，存在时优先于 `<key>`

### 字段类型规则

同一个字段名不允许在构建期与运行期有两种含义，因此：

- `targets`（可重复）：只出现在**源码 manifest**，声明要构建哪些后端，默认 `opkg`。
- `pkgmgr`（单值）：只出现在**产物 manifest**，由 `pica-pack` 写入，声明这个包属于哪个后端。

源码 manifest 声明 `pkgmgr` 会让 `pica-pack` 直接报错（应改用 `targets`）；`targets` 属于构建期字段，会在打包时从产物 manifest 中剥离，安装期不再出现。

### 必需字段

```
pkgname = <name>
appname = <name>
pkgver = <version>
os = <system>
```

以下字段有默认值，可省略：

- `pkgrel`：省略时，`pkgver = 1.0.0-2` 会拆出 `pkgrel = 2`，否则默认 `1`
- `platform`：默认 `all`
- `arch`：默认 `all`
- `pica`：省略时不校验最低 pica-cli 版本
- `targets`：默认 `opkg`

### 推荐模板

```ini
# Required
pkgname = hello
appname = hello
pkgver = 0.2.8
os = openwrt

# Optional build targets (repeatable, default: opkg)
targets = opkg
targets = apk
# targets = none

pkgrel = 1
platform = all
arch = all
pica = 0.2.8

# Optional metadata
pkgdesc = Example lifecycle package
packager = pica-pack
license = GPL-3.0-only
visibility = open
url = https://github.com/miaoermua/pica
luci_url = https://github.com/openwrt/luci/tree/master/applications/luci-app-hello
branch = stable
protocol = luci
luci_desc = LuCI plugin for hello service
source = pica

# Optional strong compatibility gate
# uname = aarch64

# Optional tags
# type = cli
# type = luci
# luci = lua1

# Install plan (repeatable, optional per-backend override)
app = hello
app_i18n = luci-i18n-hello-{lang}
base = busybox
# kmod = kmod-tun
# base@apk = busybox-apk

# Lifecycle scripts (optional, paths inside scripts/)
script_install = scripts/install
script_update = scripts/update
script_remove = scripts/remove
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

### 与安装相关的扩展字段（可重复，可选）

```
# Install plan (repeatable)
app = <native package name>
base = <native package name>
kmod = <native package name>

# Backend override (repeatable); wins over the plain key when present
app@apk = <native package name>
base@apk = <native package name>
kmod@apk = <native package name>

# Optional: i18n template for app packages
# The {lang} placeholder is resolved from pica config (default: zh-cn).
app_i18n = <native package name template>

# Program source metadata
url = <project homepage or repository URL>

# Optional LuCI source metadata
luci_url = <LuCI plugin homepage or repository URL>

# LuCI description metadata (optional)
luci_desc = <short LuCI plugin description>

# lifecycle scripts (optional)
script_install = <relative file>
script_update = <relative file>
script_remove = <relative file>

# optional type tags (repeatable)
type = cli
type = luci

# when type includes luci
luci = lua1
```

约定：

- `app/base/kmod` 定义“安装清单”，pica 安装时遍历这些字段决定需要安装的原生包。
- `<key>@<backend>` 是该后端的**整体覆盖**，不是追加：只要出现了 `<key>@<backend>`，该后端就只用带注解的那些值，未带注解的 `<key>` 全部忽略；其他后端仍用 `<key>`。
  - `base = luci-base` + `base@apk = luci-base-apk`：`apk` 装 `luci-base-apk`，`opkg` 装 `luci-base`。
  - `app@apk =`（空值）表示该后端不安装任何 `app`。
- 解析顺序为 `<key>@<backend>` → `<key>` → 空。
- `app_i18n` 允许包含 `{lang}` 占位符，pica 根据配置 `i18n` 选择实际 i18n 包名（当前仅 `zh-cn` 时参与安装/卸载）。
- `url` 用于记录程序来源（建议直接填仓库/项目地址，如 GitHub URL）。
- `luci_url` 用于记录 LuCI 插件来源（可选；仅 LuCI 包建议填写）。
- `luci_desc` 用于记录 LuCI 插件描述（与 `pkgdesc` 的通用程序描述分离）。
- `pkgmgr = opkg|apk` 时：按 `app/base/kmod` 安装，按 `app` + `app_i18n`（`i18n=zh-cn`）卸载，`pkgs/` 中的原生包交给系统包管理器。
- `pkgmgr = none` 时：不调用任何包管理器，不解析依赖，只部署 `root/` 并执行生命周期脚本。
- `root/` 按路径镜像部署到真实根文件系统，安装的文件与目录会被记录，卸载时逐条回滚。
- `script_install/script_update/script_remove` 是生命周期脚本（包内路径，一般在 `scripts/` 下），应按 POSIX `sh` 语法编写。
- 生命周期脚本由系统 `sh` 执行（OpenWrt 默认 BusyBox `sh`），不依赖 `bash`。
- 生命周期脚本保留策略（默认）：`script_remove` 保留；`script_install/script_update` 执行后不保留。
- 如果包内无任何生命周期脚本，视为无生命周期包：安装/升级/卸载均不会触发脚本执行。
- 脚本、`root/` 内文件、`pkgs/` 三者至少提供其一，否则构建与安装都会以“空包”拒绝。
- 可在 `scripts/.env` 中声明保留策略：`PICA_KEEP_SCRIPT_INSTALL/PICA_KEEP_SCRIPT_UPDATE/PICA_KEEP_SCRIPT_REMOVE/PICA_KEEP_SCRIPT_ALL`（布尔值）。
- 每次命令事务结束都会打印 `正在清理 pica 缓存`，并清理 `/var/lib/pica/cache/pkgs/` 目录内容。

- `type` 允许声明应用形态标签，便于 pica 在安装阶段做额外兼容检查。
- `type = luci` 表示“该包包含/依赖 LuCI Web UI”。如果声明了 `type = luci`，必须同时声明 `luci = lua1|js2`。
- `type = cli` 表示“该包提供纯命令行程序/脚本”。
- 仅声明 `type = cli` 时，不需要 `luci = ...`。
- `source` 用于记录安装来源（例如 `pica` / `opkg` / `local` / `url`），便于升级与排错。默认可不写；安装后 CLI 会在本地数据库里补全 `source`。

## arch（后端架构字段）

`arch` 不是 pica 自定义标签，它对应后端的架构字段。

推荐实践：

- 对绝大多数 pica 包：统一写 `arch = all`
- 仅当确实需要限制某些设备时：写具体 arch（例如 `aarch64_cortex-a53`）

`pica-cli` 的检查逻辑：

- `arch = all`：永远允许
- `arch != all`：必须出现在当前后端的架构列表中，否则安装失败
  - `opkg`：取自 `opkg print-architecture`
  - `apk`：取自 `apk --print-arch`

## os（系统类型）

`os` 用于标识系统类型（例如 `openwrt`）。

`pica-cli` 的检查逻辑：

- `os = all`：永远允许
- `os != all`：必须与当前系统类型一致，否则安装失败

## 一个 app 多个原生包

无论 opkg 还是 apk 生态，一个“应用”（你希望用一个 `pkgname` 表示）通常拆成多个原生包：

```
myapp                 # 本体二进制/服务
luci-app-myapp        # LuCI 插件
luci-i18n-myapp-zh-cn # i18n
```

因此建议：

- `pkgname` 用“应用名”做 pica 包唯一标识。
- 安装清单用 `app = ...`（以及可选 `app_i18n = ...`）列出应用子包。
- 两个后端包名不同时，用 `<key>@apk` 覆盖，例如 `base = luci-base` 配 `base@apk = luci-base-apk`。
- 卸载时 pica 按 `app` + `app_i18n` 映射处理（当前仅 `i18n=zh-cn` 时处理 i18n 包）。

## scripts/.env 预留

`scripts/` 目录下允许存在一个可选文件：

```
scripts/.env
```

用途：为生命周期脚本提供环境变量预留（默认不要求存在）。

约定：

- 安装时复制到 `/etc/pica/env.d/<pkgname>.env`
- 卸载时同步删除该 env 文件
- 可选生命周期保留控制（布尔值）：
  - `PICA_KEEP_SCRIPT_INSTALL`
  - `PICA_KEEP_SCRIPT_UPDATE`
  - `PICA_KEEP_SCRIPT_REMOVE`
  - `PICA_KEEP_SCRIPT_ALL`

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

`pica -S <selector>`/`pica -Sp`/`pica -Si` 支持以下选择器（全角符号也支持）：

```
app
app(branch)
app:branch
```

约定：

- 当前选择器仅支持按 `branch` 过滤。
- 当前不提供历史版本安装，仓库仅保留最新包。

## manifest 示例（LuCI1，双后端）

```ini
pkgname = luci-app-example
appname = example
url = https://github.com/example/example
luci_url = https://github.com/openwrt/luci/tree/master/applications/luci-app-example
branch = openwrt-23
protocol = luci
luci_desc = Example LuCI plugin for OpenWrt

pkgver = 1.0.0
pkgrel = 1
pkgdesc = Example LuCI application
packager = example
license = GPL-3.0-only
visibility = open

# 要构建的后端（可重复，默认 opkg）
targets = opkg
targets = apk

arch = all
os = openwrt
platform = amd64
uname = x86_64

pica = 0.2.8
source = pica

type = luci
luci = lua1

# Install plan (repeatable)
app = luci-app-example
app_i18n = luci-i18n-example-{lang}
base = luci-base
base = rpcd
base = ca-bundle
base@apk = luci-base-apk
kmod = kmod-tun

# Lifecycle scripts (optional)
script_install = scripts/install
script_update = scripts/update
script_remove = scripts/remove
```
