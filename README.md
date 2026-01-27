# Pica 喜鹊

Pica Is a Compact Archiver - Pica 喜鹊是一款紧凑型打包器

该名字继承自 Wine 递归浪漫，简单紧凑而不落后，以及 Arch 相似的滚动更新机制。

# todo

- 完成生命周期定义
- 完善 CLI 文档/安装方式
- 完成简单封装
- 完成标准定义
- 全流程滚动更新

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
- `pica -Ql <pkgname>`：显示已安装包 License

说明：仓库里的 CLI 实现位于 `pica-cli/pica`（bash 脚本）。更完整的包标准/字段约定见 `docs/standard.md`。

### LuCI 兼容检查（manifest）

当包强依赖某种 LuCI 实现时，建议在 `manifest` 中声明：

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

- 未包含 `type = luci`：不做 LuCI 兼容检查（即使存在 `luci = ...` 也不触发）
- 包含 `type = luci`：必须同时声明 `luci = lua1|js2`
- `pica -U`（以及部分安装路径）会尝试检测本机 LuCI 实现并匹配；无法检测或不匹配则安装失败

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

## License

GPL-3.0-only
- 用户仓库

## 特性

对于开发者来说，只需要将你的固件适配好部分依赖的 kmod 并且支持 Pica，即可以最小化的形式实现应用类似滚动更新，但是我们还是受限于 opkg 和 openwrt 的轻量化以及内核裁切特性，这是无法避免的。

- 高度自由，用户友好
- 分布式，支持用户仓库
- 小型化，基于 openwrt 深度打造

对于用户来说，你只需要使用 pica 完成包的管理，定期进行 pica 的升级，当然升级目前不支持，或者我们需要通过另外的手段实现，目前不能通过 pica 升级 pica。
