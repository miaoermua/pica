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
    pica-pkg-core/
    pica-pkg-cli/
    pica-pkg-pack/
  legacy/pica-pack/
    pica-pack
    example/
      hello/
  legacy/pica-cli/
    pica
  docs/
    standard.md
    developer.md
    user.md
```

## pica-pack（开发与测试环境构建）

### 脚本说明与调用

- 入口：`legacy/pica-pack/pica-pack`
- 职责：提供一个快速的参考实现（或生产级打包工具），开发者在提交 PR 修改打包规则前，必须通过该脚本测试本地分发行为。

### 用法

对本地存在的、符合 `standard.md` 中说明的 `staging_dir` 进行打包构建：

```bash
./legacy/pica-pack/pica-pack build <staging_dir> [--outdir DIR]
```

构建示例：
```bash
./legacy/pica-pack/pica-pack build legacy/pica-pack/example/hello --outdir /tmp/pica-test
```

输出日志风格模仿自 Arch Linux 的 `makepkg`。

## pica-cli（命令行内部机制探究）

本节说明 OpenWrt 应用命令执行框架底层的技术策略和状态设计。

### 系统层运行要求与依赖

- **基础设施**：`sh` 提供主控与生命周期解析，需保证能够正常调用 BusyBox applet。
- **环境强制依赖**：`jq`（所有后端机器解析与内部 JSON 驱动）、原生解包工具 `tar`。
- **底层后端策略**：执行 `-U/-R` 的安装和卸载操作时，通过调用系统 `opkg` 驱动 `manifest` 中填写的 `app/base/kmod` 执行最终动作。
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
  - `/usr/lib/pica/cmd/<pkgname>/` （执行后视环境变量策略定夺清扫还是滞留的生命周期脚本集）
  - `/usr/lib/pica/src/<pkgname>/` （非 `.ipk` 文件/资源分发目标目录，存放 Compose 文件或纯脚本库）

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
