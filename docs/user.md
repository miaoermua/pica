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

使用 Pica，这些碎片能被无缝封装在一个 `pica 包 (xxx.pkg.tar.gz)` 或者“应用级命名”中。Pica 会替你接管 opkg 清单与钩子工作流。

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

## 高级：非交互模式与机器化返回

当把 Pica 作为某后端的包管理驱动时，可以额外向任意指令追加格式约束：

- `--non-interactive`：禁用一切需用户键入确认的阻塞（诸如冲突警告、下载中断选项）。
- `--json` / `--json-errors`：请求命令执行后除了控制台打印，补充返回易于机器抓取解析的 JSON 数据流。
