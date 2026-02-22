# Pica 开发者文档（WIP）

本仓库当前包含两个子项目：

- `pica-pack/`：打包器（在构建机上生成 `.pkg.tar.gz`）
- `pica-cli/`：OpenWrt 侧的安装/管理器（提供 `pica` 命令）

目标是“学习 Arch/pacman 的使用方式”，但在 OpenWrt 环境里以 `opkg` 完成真正的安装/卸载动作，类似于滚动更新。

## 目录结构

```
.
  pica-pack/
    pica-pack
    examples/
      example-app/
        manifest
        binary/
        cmd/
  pica-cli/
    pica
  docs/
    standard.md
    developer.md
```

## 版本约定

- `pica-cli` 内置协议版本：`PICA_VERSION=0.0.4`
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
```

- `manifest`：包元数据（Arch-like `key = value` 文本）
- `cmd/`：必需，安装到 `/usr/bin/` 的脚本/可执行文件
- `binary/`：可选，通常放 `.ipk` 或其他二进制资源；纯脚本包可以没有该目录

`binary/` 推荐布局（多变体 ipk）：

```
binary/<platform>/<arch>/*.ipk
```

## pica-pack 输出约定

默认输出目录（不传 `--outdir`）：

```
pica-pack/bin/<pkgname>/
  <pkgname>-<pkgver>-<pkgrel>-<platform>-<arch>.pkg.tar.gz
```

当 `platform = all` 时：

```
pica-pack/bin/<pkgname>/
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
pica = <min pica-cli version>
arch = all
```

可选字段：

```
uname = <uname -m>
```

应用选择器（`-Si/-Sp`）：

```
app
app@author
app@author:version
app@author:version(branch)
```

说明：

- `version` 当前可当“分支标签”或“指定版本标识”来筛选。
- 当前仓库仍是滚动更新，不保留历史可安装包；该字段为未来历史版本能力预留。

### OpenWrt 扩展字段

```
app = <opkg-package>
base = <opkg-package>
kmod = <opkg-package>

# logical app identity (optional)
appname = <logical app name>
author = <publisher>
version = <branch-or-version-tag>
branch = <distribution branch>
protocol = <luci|cli|...>

# optional
app_i18n = luci-i18n-foo-{lang}
opkg = <opkg-package>
cmd = <relative-file-under-/usr/bin>

# optional compatibility tag
# luci = lua1
# luci = js2

# optional type tags (repeatable)
# type = cli
# type = luci

# license metadata
# license = GPL-3.0-only
# proprietary = false
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
- `app_i18n`：i18n 包名模板（`{lang}` 从配置读取，默认 `zh-cn`）。
- `opkg`：卸载阶段要 `opkg remove` 的包名（只卸载你显式列出的子包）
- `cmd`：卸载阶段要删除的 `/usr/bin/<cmd>`（只删白名单，避免误删）

### OpenWrt 的“一个 app 多个 opkg 子包”

OpenWrt 生态里，一个“应用”常常拆分为多个 opkg 包，例如：

```
myapp
luci-app-myapp
luci-i18n-myapp-zh-cn
```

推荐在同一个 pica 包的 `manifest` 中用多条 `opkg = ...` 明确列出，这样 `pica -R myapp` 只会卸载你定义的这些子包。

## pica-pack（打包器）

### 脚本

- 入口：`pica-pack/pica-pack`

### 用法

```
./pica-pack/pica-pack build <staging_dir> [--outdir DIR]
```

其中 `staging_dir` 需要包含：

```
<staging_dir>/manifest
<staging_dir>/cmd/

  # optional
  <staging_dir>/binary/
```

输出日志风格参考 Arch `makepkg`：

```
==> Making package: hello 0.1.0-1 (openwrt-any)
  -> Pica version: 0.0.4
  -> Creating archive...
==> Finished: /tmp/pica-test/hello-0.1.0-1-openwrt-any.pkg.tar.gz
```

### 示例

- 示例包 staging：`pica-pack/examples/hello/`
- 构建：

```
./pica-pack/pica-pack build pica-pack/examples/hello --outdir /tmp/pica-test
```

## pica-cli（OpenWrt 命令行）

### 依赖

- `bash`
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

开发/测试时可用环境变量覆盖路径（避免写入系统目录）：

```
PICA_ETC_DIR=/tmp/pica-etc PICA_STATE_DIR=/tmp/pica-var pica -S
```

### 命令

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

错误规则：

- 未配置软件源（`repos` 为空）会直接失败（非 0 退出）

#### 安装/更新（-U）

```
pica -U ./hello-0.1.0-1-openwrt-any.pkg.tar.gz

#### 全量升级（-Syu）

```
pica -Syu
```
```

行为：

- 解包并校验：必须包含 `manifest/cmd/binary`，`depend` 为可选
  - 校验 `pica` 协议版本一致
  - `platform`：仅展示，不作为安装门槛
  - 校验 `uname`（若提供）：优先匹配（`amd64/arm64` 与 `x86_64/aarch64` 做别名兼容）
  - 校验 `arch`：OpenWrt/opkg 架构字段，推荐 `arch = all`；若不是 all，则必须出现在 `opkg print-architecture`
- 读取 `manifest` 的安装清单字段：`kmod/base/app`（以及可选的 `app_i18n`）。
- 先处理 `kmod`：缺失/不可安装则拒绝继续。
- 再处理 `base`：软件源有则可选择走软件源；软件源没有则必须使用包内 `depend/*.ipk`，否则失败。
- 最后处理 `app`：软件源有则可选择走软件源；软件源没有则必须使用包内 `binary/*.ipk`，否则失败。
- 安装命令：将 `cmd/` 复制到 `/usr/bin/`
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

- 只删除 `manifest` 中列出的 `cmd = ...`（白名单）
- 只卸载 `manifest` 中列出的 `opkg = ...`（白名单）
- 不做依赖保护/依赖树分析（依赖策略交给 opkg 与用户）

#### 查询（-Q）

```
pica -Q
hello	0.1.0-1	openwrt-any
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
      "pkgver": "0.1.0",
      "pkgrel": "1",
      "appname": "hello",
      "author": "demo",
      "version": "rolling",
      "branch": "stable",
      "platform": "openwrt-any",
      "pica": "0.0.4",
      "filename": "hello-0.1.0-1-openwrt-any.pkg.tar.gz",
      "md5": "<md5>",
      "size": 465
    }
  ]
}
```

当前 `pica -S` 只负责下载并缓存 `repo.json` 与写入索引；后续如果要做 `-Ss/-Si/从仓库安装`，会基于该索引扩展。
