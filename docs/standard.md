# Pica 标准（草案）

本文档描述 pica 的“协议与规范”（偏标准），用于统一 `pica-pack` 与 `pica-cli` 的行为。

## 目标

- `pica-pack`：在构建机上把包打成 `pkg.tar.gz`（不使用 zst）
- `pica-cli`：在 OpenWrt 上提供 `pica` 命令，学习 Arch/pacman 的基本用法：
  - `-S` 同步（sync）
  - `-U` 安装/更新（install/update，类似 pacman -U）
  - `-R` 卸载（remove）

## 包格式（.pkg.tar.gz）

### 文件名

```
<pkgname>-<pkgver>-<platform>-<arch>.pkg.tar.gz
```

当 `platform = all` 时，为避免出现 `all-all`，文件名可简化为：

```
<pkgname>-<pkgver>-<arch>.pkg.tar.gz
```

说明：

- 文件名仍以 `platform` 作为 OpenWrt 目标代称。
- `arch` 同样保留在 `manifest` 中，用于区分 CPU/ABI 适配（例如 aarch64/a53）。

### 包内结构

压缩包根目录结构约定如下：

- `manifest`（必需，文件）
- `cmd/`（必需，目录）
- `binary/`（可选，目录）

语义：

- `cmd/`：要安装到 `/usr/bin/` 的脚本/可执行文件。
- `binary/`：可选，放 `.ipk` 等二进制资源；纯脚本包（exec）可以完全不提供 `binary/`。

`binary/` 推荐结构（多变体）：

```
binary/<platform>/<arch>/*.ipk
```

打包器会把每个 `<platform>/<arch>` 组合单独生成一个 pica 包。

## pica-pack 输出目录约定

默认情况下（不传 `--outdir`），`pica-pack` 会输出到 `pica-pack/bin/<pkgname>/`：

```
pica-pack/bin/<pkgname>/<pkgname>-<pkgver>-<platform>-<arch>.pkg.tar.gz
```

当 `platform = all` 时：

```
pica-pack/bin/<pkgname>/<pkgname>-<pkgver>-<arch>.pkg.tar.gz
```

## 兼容维度：uname + arch（platform 仅展示）

为了让安装/更新行为尽可能稳定，pica 的“实际兼容性判断”优先使用：

- `uname`：与 `uname -m` 严格匹配（跨系统最通用的基线）
- `arch`：OpenWrt/opkg 定义的架构字段（可通过 `opkg print-architecture` 查看），推荐统一使用 `all`

`platform` 仍然保留，但它只用于应用商店/仓库展示与筛选，不作为安装的硬性门槛。

在需要展示时（日志、查询）应同时展示 `platform + arch + uname`（若 `uname` 未提供则可省略）。

## manifest（Arch-like 文本）

### 格式

- `key = value` 一行一个字段
- 支持 `#` 注释
- 支持重复 key（表示数组）

### 必需字段

```
pkgname = <name>
pkgver = <version-release>
platform = all
arch = all
pica = <min pica-cli version>
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
packager = ...
builddate = <unix timestamp>
size = <bytes>

# license metadata
license = GPL-3.0-only
proprietary = false
```

约定：

- `builddate`：可选；Unix 时间戳（秒）。推荐不要在“源码 manifest”里手写，由 `pica-pack build` 在构建产物中自动补全。
- `size`：可选；字节数（bytes）。推荐不要在“源码 manifest”里手写，由 `pica-pack build` 在构建产物中自动计算并补全。

### 与 OpenWrt 安装相关的扩展字段（可重复，可选）

```
depend = <opkg package name>
opkg = <opkg package name>
cmd = <relative file>

# special deps (repeatable)
base_depend = <opkg package name>
kmod_depend = <opkg package name>

# optional type tags (repeatable)
type = cli
type = luci

# when type includes luci
luci = lua1
```

约定：

- `type` 允许声明应用形态标签，便于 pica 在安装阶段做额外兼容检查。
- `type = luci` 表示“该包包含/依赖 LuCI Web UI”。如果声明了 `type = luci`，必须同时声明 `luci = lua1|js2`。
- `type = cli` 表示“该包提供纯命令行程序/脚本”。目前 `type = cli` 主要用于元数据标注，pica-cli 不会因为缺少 LuCI 而拒绝安装；需要 LuCI 的包请务必使用 `type = luci` 明确标注。

- `depend`：安装阶段 `opkg install` 的依赖（不做依赖树管理）。
- `opkg`：卸载阶段 `opkg remove` 的包名（仅卸载你显式列出的包）。
- `cmd`：卸载阶段删除的 `/usr/bin/<cmd>` 白名单（避免误删）。
- `base_depend`：基础依赖（必须已安装；`pica` 只检查，不自动安装）。
- `kmod_depend`：kmod 依赖（必须已安装；`pica` 只检查，不自动安装）。

说明：

- `depend` 是“安装阶段由 pica 触发 opkg 安装”的依赖。
- `base_depend/kmod_depend` 是“运行/内核环境前置条件”，默认要求系统里已存在；同步（`-S`）和安装（`-U` / `-Sp`）阶段会做检查并在缺失时报错/告警。

## arch（OpenWrt/opkg）

`arch` 不是 pica 自定义标签，它对应 OpenWrt/opkg 的架构字段。

推荐实践：

- 对绝大多数 pica 包：统一写 `arch = all`
- 仅当确实需要限制某些设备时：写 opkg 的具体 arch（例如 `aarch64_cortex-a53`）

`pica-cli` 的检查逻辑：

- `arch = all`：永远允许
- `arch != all`：必须出现在 `opkg print-architecture` 输出中，否则安装失败

## OpenWrt：一个 app 多个 opkg 包

OpenWrt/LEDE 生态里，一个“应用”（你希望用一个 `pkgname` 表示）通常拆成多个 opkg 包：

```
myapp                 # 本体二进制/服务
luci-app-myapp        # LuCI 插件
luci-i18n-myapp-zh-cn # i18n
```

因此建议：

- `pkgname` 用“应用名”做 pica 包唯一标识。
- 用多条 `opkg = ...` 列出该应用对应的 opkg 子包（core/luci/i18n…）。
- 卸载时 pica 只卸载 `opkg =` 明确列出的包，不猜测、不扩展。

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

## 许可证（license / proprietary）

manifest 中允许定义许可证信息：

```
license = GPL-3.0-only
proprietary = false
```

约定：

- `license`：建议用 SPDX 标识（例如 `GPL-3.0-only`、`MIT`、`Apache-2.0`）
- `proprietary`：`true|false`，用于标记是否为专有软件
- 当前版本只做“定义与展示”，不做任何许可证强制校验

## LICENSE 文件（包内可选）

打包时允许在 staging 根目录放一个 `LICENSE` 文件，`pica-pack` 会把它原样打进压缩包根目录：

```
LICENSE
```

当前版本不自动安装/展开该文件，仅作为后续 `pica` 命令显示许可证内容的基础。

## manifest 示例（LuCI1）

```
pkgname = luci-app-example
pkgver = 1.0.0
pkgrel = 1

pkgdesc = Example LuCI application
url = https://example.com
packager = example

arch = all
platform = openwrt
uname = x86_64

pica = 0.0.25

type = luci
luci = lua1

depend = luci-base
depend = rpcd

opkg = luci-app-example
opkg = luci-i18n-example-zh-cn

cmd = example-cli

base_depend = ca-bundle
kmod_depend = kmod-tun

license = GPL-3.0-only
proprietary = false
```
