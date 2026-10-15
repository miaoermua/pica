# Pica 用户指引（User Guide）

> [!WARNING]
>
> 🚧 **阶段性警告 (WIP)** 🚧
>
> Pica 现阶段正处于**开发版本**进程中。
> 其命令行交互、内部工作架构与打包协议尚在设计调整周期内。**开发团队当前无法对此工具或它托管安装的软件包提供任何最终品质保证（No Warranty）。** 
> 强烈建议您仅在测试环境或能够承担配置损毁风险的开发设备上使用它。对于不可预知的错误导致的数据丢失或系统异常，我们概不负责。

## 简介

**Pica** 是一款致力于为 OpenWrt 提供类似 Arch Linux 的“Pacman 风格”体验的包管理器。

在传统的 OpenWrt 环境中，哪怕安装一个简单外部提供的带 LuCI 界面的应用，用户也经常要在核心进程 `.ipk`、语言包 `luci-i18n-*` 以及接口插件 `luci-app-*` 中反复奔波，并处理琐碎的碎片化操作。

使用 Pica，这些碎片能被无缝封装在一个 `pica 包 (xxx.pkg.tar.gz)` 或者“应用级命名”中。Pica 会替你接管后端清单（`opkg` / `apk` / `pica`）与钩子工作流。

## 核心使用方式

Pica 主打类 pacman 格式的精简参数。

### 1. 同步软件源 (`-S`)

与主配置中定义好的远程（或内网局域网）仓库同步数据索列表：

```bash
pica -S
```

输出示例：
```text
:: Synchronizing package databases...
:: main downloading...
:: main updated
```

### 2. 通过软件源安装 (`-S <pkg>`)

安装仓库内已知的软件：

```bash
pica -S miaoer-app
```

> 想要确认应用信息再安装？可以通过 `pica -Si miaoer-app` 快速检视。

### 3. 本地直装 / URL 在线安装 (`-U`)

如果有人发送了你一个构建好的 pica 包，或是你在网页版应用商店复制了安装包的直接下载链接，均能通过 `-U` (Update/Upload) 完成安装：

```bash
# 从本地路径安装
pica -U /tmp/miaoer-app-0.2.8-1-all.pkg.tar.gz

# 从网络直链安装
pica -U https://example.com/downloads/miaoer-app-0.2.8-1-all.pkg.tar.gz
```

### 4. 一键卸载应用 (`-R`)

按“应用包”的粒度将关联资源和子插件一并移除，再也不用逐条运行 `opkg remove`：

```bash
pica -R miaoer-app
```

输出示例：
```text
:: Removing miaoer-app...
:: removing opkg package: miaoer-app
:: removing opkg package: luci-app-miaoer-app
:: Transaction completed
```

### 5. 查询机器状态 (`-Q`)

查看本机已被 Pica 托管接管的已安装应用：

```bash
pica -Q
```

> 若该包由 `root/` 载荷部署，Pica 会按安装时记录的路径逐条回滚，不会调用包管理器。

## 高级：指定安装后端

默认情况下 Pica 按 `--pkgmgr` > 包 manifest > `/etc/pica/pica.json` > 自动探测（`apk` 优先于 `opkg`）的顺序决定后端。需要强制指定时：

```bash
pica --pkgmgr apk -S miaoer-app
pica --pkgmgr pica -U /tmp/miaoer-app-0.2.8-1-all-pica.pkg.tar.gz
```

- `opkg` / `apk`：把包内 `pkgs/` 交给对应的系统包管理器安装。
- `pica`：Pica 自己解包 `pkgs/` 中的 `.ipk` / `.apk`，不依赖系统包管理器。

> 安装若中途失败，Pica 会回滚已装的依赖与已铺放的文件；若回滚也无法完成，会列出残留路径供手动清理。

### apk 后端安装本地包

`pkgs/` 里的 `.apk` 是本地文件而非仓库软件包，Pica 会以本地文件的方式交给 apk：

- 默认带上 `--force-non-repository`：OpenWrt 25.12 起的 apk-tools 3.x 会拒绝不属于任何仓库的软件包。Pica 先读 `apk --version`，2.x 不传该参数（旧版不认识它）。
- 同时带 `--allow-untrusted`：豁免 apk 的签名检查，信任的是 Pica 归档本身；按名字从仓库安装的依赖包不受影响，仍走 apk 自己的仓库与签名。

若你的 apk 不认这个参数，可在 `/etc/pica/pica.json` 里关掉：

```json
{
  "apk_force_non_repository": false
}
```

### 包内的本地容器如何安装

一个产物的 `pkgs/` 是**一整批**本地容器，Pica 把它作为一次请求交给包管理器，且整个安装过程中只装一次：

- 容器之间可以互相依赖（`aaa-app` 依赖同批的 `zzz-core`），所以它们必须在**同一条命令**里；包管理器只在同一次请求给出的文件之间解析依赖，逐个安装会让字典序决定成败。
- 依赖走仓库还是走包内容器，由 `--feed-policy` 对整包**判定一次**；走仓库时仍是先 `base` 后 `app`（每个阶段各自一次请求）。
- `kmod` 始终从仓库装，且只装本机缺的那些。

若包内 `pkgs/` 目录存在但没有任何包，安装会在校验阶段直接失败（不会装一半）。

## 高级：非交互模式与机器化返回

当把 Pica 作为某后端的包管理驱动时，可以额外向任意指令追加格式约束：

- `--non-interactive`：禁用一切需用户键入确认的阻塞（诸如冲突警告、下载中断选项）。
- `--json` / `--json-errors`：请求命令执行后除了控制台打印，补充返回易于机器抓取解析的 JSON 数据流。
