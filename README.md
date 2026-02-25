# Pica 喜鹊

Pica Is a Compact Archiver - Pica 喜鹊是一款紧凑型打包器

![rust](https://ziadoua.github.io/m3-Markdown-Badges/badges/Rust/rust1.svg)
![shell](https://ziadoua.github.io/m3-Markdown-Badges/badges/Shell/shell1.svg)
![licence](https://ziadoua.github.io/m3-Markdown-Badges/badges/LicenceGPLv3/licencegplv31.svg)

该名字继承自 Wine 递归浪漫，简单紧凑而不落后，运用 Arch 系包管理器相似的滚动更新机制，在 OpenWrt 上体验强大统一且先进的软件包管理器。

## 特性

> 说明：Bash 版本已归档到 `legacy/`，后续以 Rust 核心实现为主。

对于开发者来说，只需要将你的固件适配好部分依赖的 kmod 并且支持 Pica，即可以最小化的形式实现应用类似滚动更新特性，Pica 还是受限于 openwrt 包管理器和 openwrt 的轻量化以及内核裁切特性，这是无法避免的。

- 高度自由：用户友好，全开源，自建用户仓库
- 功能强大：支持多分支应用声明、生命周期管理以及多种 OpenWrt 分支
- 小巧玲珑：基于 openwrt 深度打造，设计时就考虑体积敏感


---

## CLI（当前支持）

- `pica -S`：同步 pica 仓库索引（repo.json -> index.json）
- `pica -S <appname>`：按应用名安装（默认：自动在 opkg/pica 源之间决策）
- `pica -Su`：升级所有已安装的 pica 包（从 index.json 选择最新版本并安装）
- `pica -Syu`：先 `-S` 再 `-Su`
- `pica -Si <appname>`：显示仓库远端包信息（基于已同步 index）
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

补充：`pkgmgr=none` 的生命周期包可使用 `src/` 携带原始资源，安装时会落地到 `/usr/lib/pica/src/<pkgname>/`，供 `cmd_install/cmd_update/cmd_remove` 使用。

## Pica 能力

- 当前标准归档类型：`*.pkg.tar.gz`（pica 自有封装格式）。
- 现阶段安装后端：`opkg` 或 `none`（生命周期脚本模式）。
- 包内载荷可包含 `binary/`（常见为 ipk）、`depend/`、`src/`、`cmd/`。
- `src/` 可承载未编译原始资源（例如：脚本、luci 模板、`docker-compose.yml`），由生命周期脚本按需部署。
- 未来规划：可能增加对 `apk` 生态的兼容输入/封装能力（以正式发布版本为准），并且完善健全 Rust 分支，放弃 Bash 原分支。
- 非目标：Pica 不提供 Docker 管理服务，不负责容器编排、守护进程管理或 Compose 生命周期托管。

对于用户来说，你只需要使用 pica 完成包的管理，定期进行 pica 的升级。

## 配置文件

默认配置文件：`/etc/pica/pica.json`（首次运行会自动创建）。

最小示例：

```json
{
  "repos": [],
  "i18n": "zh-cn"
}
```

- `repos[]`：pica 仓库列表（`pica -S` 同步与 `pica -Si/-Sp` 查询/安装使用）
- `i18n`：默认 LuCI i18n 语言（用于安装 `luci-i18n-<app>-<lang>`，不影响 pica 自身输出语言）

---

更多有关 Pica 信息 [请参考文档](./docs/)

## 感谢

[@Canmi](https://github.com/canmi21)

此项目的诞生借鉴了这些优秀的开源项目：

- Paru
- Pacman
