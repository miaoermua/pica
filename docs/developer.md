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
    pica-pkg-cli/     pica 命令（安装/卸载/查询/同步/初始化向导/后端）
                       src/i18n.rs 放界面语言与全部用户可见文本
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
cargo run -p pica-pkg-pack -- build <pkgdir> [--outdir DIR] [--pkgmgr opkg|apk|pica|opkg,apk]
```

`<pkgdir>` 目录结构：

```text
manifest                           必需，key = value
scripts/                           可选，生命周期脚本 install/update/remove/.env
root/                              可选，按路径镜像到目标根文件系统
pkgs/<backend>/                    可选，交给 opkg/apk 安装的容器（backend 不含 pica）
pkgs/<backend>/<platform>/<arch>/  可选，变体矩阵写法
LICENSE                            可选
```

构建目标来自 manifest 里的 `targets`（可重复或逗号分隔），也可用 `--pkgmgr` 覆盖，默认 `opkg`。
每个 `<platform>/<arch>` 组合产出一个归档，`pkgs/` 会被拍平成该组合的内容。

`pica` 目标只收 `scripts/` 与 `root/`：`pkgs/pica/` 会让构建直接失败，因为 pica 不读容器格式——展开容器会把文件的所有权从后端数据库里抢走，也丢掉后端的依赖求解（理由见 `standard.md` 的「`pica` 后端」一节）。`pica-pack` 仍会读 `.ipk` / legacy `.apk` 统计 `size`，读不出来（apk-tools 3.x 的 ADB）只告警并跳过。

产物命名与输出目录：

```text
<pkgname>-<pkgver>-<pkgrel>-<platform>-<arch>[-opkg|-apk|-pica].pkg.tar.gz
platform = all 时省略 platform 段

<outdir>/opkg/    opkg
<outdir>/apk/     apk
<outdir>/pica/    pica
```

输出日志风格模仿自 Arch Linux 的 `makepkg`。

## pica-cli（命令行内部机制探究）

本节说明 OpenWrt 应用命令执行框架底层的技术策略和状态设计。

### 系统层运行要求与依赖

- **基础设施**：`sh` 提供主控与生命周期解析，需保证能够正常调用 BusyBox applet。
- **环境强制依赖**：原生解包工具 `tar`（JSON 解析、状态存储与校验均由 Rust 侧实现，不再依赖 `jq`）。
- **底层后端策略**：执行 `-U/-R` 的安装和卸载操作时，`opkg` / `apk` 后端通过调用系统包管理器驱动 `manifest` 中填写的 `app/base/kmod` 执行最终动作；`pkgmgr = pica` 时由 Pica 部署 `root/`、执行生命周期脚本；这类归档不携带 `pkgs/`，带了会在写入任何文件之前被拒绝。
- **公网连接支持优先级**：`curl` -> `wget` -> `uclient-fetch`。前两者被要求不要自动跟随重定向，由 `fetch_url` 逐跳检查目标后再决定是否继续；`uclient-fetch` 无法在跟随前报告 `Location`，因此退化为单次抓取（其自身重定向上限生效），这条路径上的包仍受索引 `sha256` 约束。`file://` 源不需要任何下载器，工具在第一次真正需要下载时才探测。

### 状态目录（VFS Layout）

Pica 的环境与运行时状态持久化设计：

- **配置文件（读）**：
  - `/etc/pica/pica.json`
  - `/etc/pica/env.d/` （供隔离的组件环境变量注入）
- **本地服务状态 / RDBMS 映射（读写）**：
  - `/var/lib/pica/db.json` （核心登记表，维护机器已安装应用树；条目里的 `source` 记渠道、`pkgmgr` 记实际后端，语义与迁移规则见 `standard.md`）
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

### 界面语言（i18n）

- 语言与文本都在 `src/i18n.rs`：`Lang`（`EnUs` / `ZhCn`）负责解析代码、决定配置读写；`Key` 是唯一的文本键，`Lang::text(key)` 取正文，`Lang::args(key, &[...])` 先替换 `{0}`/`{1}` 再用。业务代码里不写语言判断，也不写裸的英文句子。
- 语言来源优先级：`--lang` > `pica.json` 的 `language` > 英文。`--lang` 只在本次运行生效，不回写配置；写错的代码或未知语言一律按英文继续，不让一个手写配置把工具变成砖头。
- 加一条文案：在 `Key` 里加一个变体，在 `english()` / `chinese()` 两张表各补一行，并加进 `ALL_KEYS`。两张表都是穷尽 `match`，漏一处编译不过；`ALL_KEYS` 驱动的测试还会检查两边的 `{n}` 占位符个数一致。
- 不翻译的东西：包名、命令、配置键、协议字段、文件路径、`E_*` 错误码，以及 `--help` / `--version` / `-Qi` 的字段名。`--json-errors` 的 `code` 恒定，只有 `message` 随语言变。
- 第三方工具的错误原文不丢：本地化的只是外层句式，原始输出（`opkg`/`apk` 的输出、`io::Error` 的文本）作为参数塞进占位符。

### 命令底层微观事务工作流

- **同步（-S）**：读取配置表的 `url`（源根地址）-> 按该源的 `backends` 抓 `<root>/index.json` 入口描述与对应后端索引 -> 校验后写入 `state/index.json` 的 `repos.<name>`（`backends` / `entry` / `indexes`）。`index.json` 是唯一入口：缺失、读不到或不合规都直接失败，不读 `<root>/repo.json`，也没有旧格式回退分支。
- **初始化（`pica setup`）**：`src/menu.rs` 提供上下键菜单（终端上切 raw 模式逐字节读，非终端退回编号输入，`Terminal` 在 `Drop` 里用 `stty <saved>` 还原行设置）；`src/commands/setup.rs` 按"语言 -> 软件源 -> 抓入口描述求后端交集 -> 同步"推进。语言菜单的标签是各语言的自称（`English (en-US)`），不随界面语言翻译。任何一步失败都不落盘：`sync::build` 先把配置、索引与缓存全部算出，`state::Transaction` 再一次性提交，写入失败按逆序恢复已写文件，恢复失败时错误消息同时带上原始原因。
- **地址与策略（`src/source.rs`）**：`join_child` 把源发布的相对路径拼到根地址上，`join_sibling` 把入口描述给的 manager path 拼到描述文件所在目录，两者都要求安全相对路径；`FetchPolicy` 决定哪些 scheme 与主机可被抓取（公网明文默认拒绝、局域网允许、`file://` 只对本地源开放、重定向禁止 https→http），下载与同步共用同一份判定。
- **索引刷新防抖（-U 生命周期部分）**：
  - 当通过 `-S` 引发的远程依赖（base/app/kmod）安装前发现缺少依赖必须拉外网资源执行 `opkg install` 之前。
  - Pica 会先探测本机的依赖索引状态，只有缺索引才会调用昂贵的 `opkg update` / `apk update`：`opkg` 精确到每个 feed 的列表文件（`lists_dir` 下的同名文件），`apk` 精确到每个仓库的缓存文件（`APKINDEX.<索引 URL 的 sha256 前 4 字节>.tar.gz`），因此能说出**是哪一个源**没有索引。因为缓存文件名就是索引 URL 的摘要，仓库列表或 `arch` 一改，期望的文件名随之改变，不需要再比较修改时间。
  - `pkgmgr/index.rs` 是这条规则的唯一实现：`missing_indexes` 说缺什么、`ensure_index` 决定要不要刷新。只有"缺索引且刷新没补上"（断网、源失效、`opkg` 锁被活进程持有）才报 `E_INDEX_UNAVAILABLE`，消息里带上刷新命令的退出码与输出。少了索引就等于答不出"源里有没有这个包"，绝不能当成"源里没有"。
- **状态审计落盘（Install-Report）**：每次成功的安装流完成，不仅 `db.json` 被追加对象关系。它会在 `/var/lib/pica/` 生成一份详细 JSON，用以记录（新增了哪些库，kmod 层补充了哪些依赖）的事务安装审计快照。
