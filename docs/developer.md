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

- `pica-cli` 内置协议版本：`PICA_VERSION=0.0.1`
- 每个 pica 包的 `manifest` 必须声明：`pica = 0.0.1`
- `pica -U` 安装时会校验 `manifest` 的 `pica` 与 CLI 是否一致；不一致直接失败（非 0 退出）。

## pica 包格式（.pkg.tar.gz）

### 文件名

```
<pkgname>-<pkgver>-<platform>.pkg.tar.gz
```

说明：

- 使用 `platform` 代替 `arch`。
- `pkgver` 推荐形如 `1.2.3-1`（语义版本 + release）。

### 包内固定结构

压缩包根目录固定只有这三项：

```
manifest
binary/
cmd/
```

- `manifest`：包元数据（Arch-like `key = value` 文本）
- `binary/`：可选，通常放 `.ipk` 或其他二进制资源
- `cmd/`：可选，通常放要安装到 `/usr/bin/` 的脚本/可执行文件

## manifest（Arch-like 文本）

### 格式规则

- 一行一个字段：`key = value`
- 支持 `#` 注释
- 支持重复 key（在 CLI 中会变成数组）

### 必需字段

```
pkgname = <name>
pkgver = <version-release>
platform = <platform>
pica = 0.0.1
```

### OpenWrt 扩展字段

```
depend = <opkg-package>
opkg = <opkg-package>
cmd = <relative-file-under-/usr/bin>
```

语义：

- `depend`：安装阶段要 `opkg install` 的依赖（不做依赖树管理）
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
<staging_dir>/binary/
<staging_dir>/cmd/
```

输出日志风格参考 Arch `makepkg`：

```
==> Making package: hello 0.1.0-1 (openwrt-any)
  -> Pica version: 0.0.1
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
- `wget`
- `opkg`（仅 `-U/-R` 需要）

### 安装位置与文件

- 配置目录：`/etc/pica/`
- 配置文件：`/etc/pica/pica.json`
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
```

行为：

- 解包并校验：必须包含 `manifest/binary/cmd`
- 校验 `pica` 协议版本一致
- 校验 `platform`：
  - 允许 `openwrt-any|any|all`
  - 或者必须等于本机 `detect_platform()`（优先取 `/etc/openwrt_release` 的 `DISTRIB_TARGET`）
- 安装依赖：对 `depend` 逐条执行 `opkg install <name>`
- 安装 ipk：对 `binary/*.ipk` 执行 `opkg install <file>`
- 安装命令：将 `cmd/` 复制到 `/usr/bin/`
- 写入本地安装数据库：`/var/lib/pica/db.json`

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
    <pkgname>-<pkgver>-<platform>.pkg.tar.gz
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
      "pkgver": "0.1.0-1",
      "platform": "openwrt-any",
      "pica": "0.0.1",
      "filename": "hello-0.1.0-1-openwrt-any.pkg.tar.gz",
      "sha256": "<sha256>",
      "size": 465
    }
  ]
}
```

当前 `pica -S` 只负责下载并缓存 `repo.json` 与写入索引；后续如果要做 `-Ss/-Si/从仓库安装`，会基于该索引扩展。

