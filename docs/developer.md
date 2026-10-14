# Pica 开发者文档（WIP）

本仓库当前包含两个子项目：

- `crates/pica-pkg-pack/`：构建机打包器（生成 `.pkg.tar.gz`，提供 `pica-pack` 命令）
- `crates/pica-pkg-cli/`：OpenWrt 安装/管理的 CLI 前端（提供 `pica` 命令）

> [!NOTE]
>
> 💡 寻找包格式协议？<br/>
> 关于打包规则、`.pkg.tar.gz` 的内部结构定义、`manifest` 文件规范说明与 `repo.json` 强约束条件，请统一阅读 [Pica 标准规范 (standard.md)](./standard.md)。
>
> 💡 寻找使用方法？<br/>
> 了解命令行工具的基础用法及防风险警告，请阅读 [Pica 用户指引 (user.md)](./user.md)。

---

## 目录结构

```text
.
  crates/
    pica-pkg-core/    归档、manifest、repo.json 与基础 IO
    pica-pkg-cli/     pica 命令（安装/卸载/查询/同步/后端）
    pica-pkg-pack/    pica-pack 命令（构建与打包）
  docs/
    standard.md
    developer.md
    user.md
```

## pica-pack（开发与测试环境构建）

### 脚本说明与调用

- 入口：`cargo run -p pica-pkg-pack --`（产物二进制为 `pica-pack`）
- 职责：把符合 `standard.md` 的 `pkgdir` 打成 `.pkg.tar.gz`，并按后端生成 `repo.json` 与 `SHA256SUMS`。

### 用法

```bash
cargo run -p pica-pkg-pack -- build <pkgdir> [--outdir DIR] [--pkgmgr opkg|apk|none|opkg,apk]
```

`<pkgdir>` 目录结构：

```text
manifest                           必需，key = value
scripts/                           可选，生命周期脚本 install/update/remove/.env
root/                              可选，按路径镜像到目标根文件系统
pkgs/<backend>/                    可选，该后端的系统包
pkgs/<backend>/<platform>/<arch>/  可选，变体矩阵写法
LICENSE                            可选
```

构建目标来自 manifest 里的 `targets`（可重复或逗号分隔），也可用 `--pkgmgr` 覆盖，默认 `opkg`。
每个 `<platform>/<arch>` 组合产出一个归档，`pkgs/` 会被拍平成该组合的内容。

产物命名与输出目录：

```text
<pkgname>-<pkgver>-<pkgrel>-<platform>-<arch>[-apk|-none].pkg.tar.gz
platform = all 时省略 platform 段

<outdir>/opkg/    opkg
<outdir>/apk/     apk
<outdir>/none/    none
```

输出日志风格模仿自 Arch Linux 的 `makepkg`。

## pica-cli（命令行内部机制探究）

本节说明 OpenWrt 应用命令执行框架底层的技术策略和状态设计。

### 系统层运行要求与依赖

- **基础设施**：`sh` 提供主控与生命周期解析，需保证能够正常调用 BusyBox applet。
- **环境强制依赖**：原生解包工具 `tar`（JSON 解析、状态存储与校验均由 Rust 侧实现，不再依赖 `jq`）。
- **底层后端策略**：执行 `-U/-R` 的安装和卸载操作时，通过调用系统包管理器（`opkg` 或 `apk`）驱动 `manifest` 中填写的 `app/base/kmod` 执行最终动作；`pkgmgr = none` 时不调用任何包管理器，仅部署 `root/` 并执行生命周期脚本。
- **公网连接支持优先级**：`uclient-fetch` -> `wget` -> `curl`。

### 状态目录（VFS Layout）

Pica 的环境与运行时状态持久化设计：

- **配置文件（读）**：
  - `/etc/pica/pica.json`
  - `/etc/pica/env.d/` （供隔离的组件环境变量注入）
- **本地服务状态 / RDBMS 映射（读写）**：
  - `/var/lib/pica/db.json` （核心登记表，维护机器已安装应用树）
  - `/var/lib/pica/index.json`（已同步的所有在线 Repo 资源目录）
  - `/var/lib/pica/cache/repos/<name>.json`
  - `/var/lib/pica/db.lck` （并发安全锁源）
- **运行期临时文件（缓存）**：
  - 下载目标存储定位 `/var/lib/pica/cache/pkgs/*.pkg.tar.gz`（受保护，成功和失败回滚都会主动清理以释放 Flash）。
- **静态资源部署桩**：
  - `/usr/lib/pica/scripts/<pkgname>/` （执行后视环境变量策略定夺清扫还是滞留的生命周期脚本集）
  - `root/` 内的文件按路径镜像部署到真实根文件系统，卸载时按 `db.json` 记录逐条回滚

### 运行时并发约束锁设计

OpenWrt 的包管理容忍度低（尤其是修改同一个清单或写入），故 Pica 要求在执行前持有**全局运行锁**：

1. **原子建立检测**：使用新建具有时序的目录锁 `db.lck.d`。
2. **死进程僵尸清扫**：如果探测到此级系统锁的原始 PID 失去存活响应，Pica 具备自动洗地清除残余并再次抢夺写锁的修复机制。
3. **`opkg.lock` 同步规避**：不仅对自身排队，如果系统内发生了 `opkg` 文件抢夺造成 `pica -U` 执行阻塞，如果 `opkg.lock` 宿主为死进程，也会尝试唤起并重试。

### 操作环境覆盖（用于外部开发机调试）

为允许开发者在非 OpenWrt 设备（如 Ubuntu 开发机）上快速跑通代码检查分支构建的异常逻辑：可避免直接写入当前计算机的根目录层造成毁伤：

```bash
PICA_ETC_DIR=/tmp/pica-etc PICA_STATE_DIR=/tmp/pica-var pica -S
```

### 命令底层微观事务工作流

- **同步（-S）**：单纯读取配置表的 `url` 到临时队列 -> 下发至后端请求工具并发或串行抓取 `repo.json`。
- **索引刷新防抖（-U 生命周期部分）**：
  - 当通过 `-S` 引发的远程依赖（base/app/kmod）安装前发现缺少依赖必须拉外网资源执行 `opkg install` 之前。
  - Pica 会先探测本机的依赖索引目录状态（如 OpenWrt 内存系统的 `/var/opkg-lists/` 是否填满），只有空置才会调用昂贵的 `opkg update`。
- **状态审计落盘（Install-Report）**：每次成功的安装流完成，不仅 `db.json` 被追加对象关系。它会在 `/var/lib/pica/` 生成一份详细 JSON，用以记录（新增了哪些库，kmod 层补充了哪些依赖）的事务安装审计快照。
