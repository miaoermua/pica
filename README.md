# Pica 喜鹊

Pica Is a Compact Archiver - Pica 喜鹊是一款紧凑型打包器

![rust](https://ziadoua.github.io/m3-Markdown-Badges/badges/Rust/rust1.svg)
![shell](https://ziadoua.github.io/m3-Markdown-Badges/badges/Shell/shell1.svg)
![licence](https://ziadoua.github.io/m3-Markdown-Badges/badges/LicenceGPLv3/licencegplv31.svg)

该名字继承自 Wine 递归浪漫，简单紧凑而不落后，以及运用 Arch 系包管理器相似的滚动更新机制。

> 说明：Bash 版本已归档到 `legacy/`，后续以 Rust 核心实现为主。
# todo

- 完善 CLI 文档/安装方式
- 完成简单封装
- 完成标准定义
- 全流程滚动更新

## 特性

对于开发者来说，只需要将你的固件适配好部分依赖的 kmod 并且支持 Pica，即可以最小化的形式实现应用类似滚动更新特性，Pica 还是受限于 openwrt 包管理器和 openwrt 的轻量化以及内核裁切特性，这是无法避免的。

- 高度自由，用户友好功能强大
- 分布式，可自建用户仓库
- 前瞻性，支持多分支应用声明、生命周期管理
- 小型化，基于 openwrt 深度打造

对于用户来说，你只需要使用 pica 完成包的管理，定期进行 pica 的升级，当然升级目前不支持，或者我们需要通过另外的手段实现，目前不能通过 pica 升级 pica。

---

## CLI（当前支持）

- `pica -S`：同步 pica 仓库索引（repo.json -> index.json）
- `pica -Su`：升级所有已安装的 pica 包（从 index.json 选择最新版本并安装）
- `pica -Syu`：先 `-S` 再 `-Su`
- `pica -Si <appname>`：按应用名安装（默认：如果 opkg 源有则询问，否则走 pica 源）
- `pica -So <appname>`：强制走 opkg 安装（会尝试安装 `app/luci-app-*/luci-i18n-*`）
- `pica -Sp <appname>`：强制走 pica 镜像源安装（从 repo.json 解析并下载 pkg.tar.gz）
- `pica -U <pkgfile>`：从本地 `.pkg.tar.gz` 安装/更新（类似 pacman -U）
- `pica -R <pkgname>`：卸载（不处理依赖关系）
- `pica -Q`：列出已安装的 pica 包
- `pica -Qi <pkgname>`：显示已安装包信息
- `pica -Ql <pkgname>`：列出该包安装到系统的文件路径
- `pica --fetch-timeout <seconds>`：下载超时（默认 30 秒）
- `pica --fetch-retry <count>`：下载重试次数（默认 2，表示总尝试次数=1+count）
- `pica --fetch-retry-delay <seconds>`：重试间隔（默认 1 秒）

说明：当前核心实现为 Rust（`crates/pica-cli-rs` / `crates/pica-pack-rs`），Bash 版本已归档到 `legacy/`（不再作为核心维护）。更完整的包标准/字段约定见 `docs/standard.md`。

## 配置文件

默认配置文件：`/etc/pica/pica.json`（首次运行会自动创建）。

最小示例：

```json
{
  "repos": [],
  "i18n": "zh-cn"
}
```

- `repos[]`：pica 仓库列表（`pica -S` / `pica -Sp` 使用）
- `i18n`：默认 LuCI i18n 语言（用于安装 `luci-i18n-<app>-<lang>`，不影响 pica 自身输出语言）

## GitHub Actions（OpenWrt Rust 二进制）

仓库已提供工作流：`.github/workflows/openwrt-rust-binaries.yml`。

- 触发方式：
  - 手动触发（`workflow_dispatch`）
  - 推送 `v*` tag（例如 `v0.1.27`）
  - Pull Request（仅在 Rust/CI 相关文件变更时）
- 目标平台（musl，适配 OpenWrt 常见架构）：
  - `x86_64-unknown-linux-musl`
  - `aarch64-unknown-linux-musl`
  - `armv7-unknown-linux-musleabihf`
- 产物内容：每个目标输出一个 `tar.gz`，内含：
  - `pica-rs`
  - `pica-pack-rs`
- 发布行为：当推送 `v*` tag 时，自动把构建产物上传到 GitHub Release。
