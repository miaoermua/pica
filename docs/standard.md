# Pica 标准（草案）

本文档描述 pica 的“协议与规范”（偏标准），用于统一 `pica-pack` 与 `pica-cli` 的行为。

## 目标

- `pica-pack`：在构建机上把包打成 `pkg.tar.gz`（不使用 zst）
- `pica-cli`：在 OpenWrt 上提供 `pica` 命令，学习 Arch/pacman 的基本用法：
  - `-S` 同步（sync）
  - `-U` 安装/更新（install/update，类似 pacman -U）
  - `-R` 卸载（remove）

## 归档类型与载荷边界

- 当前唯一标准归档类型：`*.pkg.tar.gz`。
- `pkgs/` 里可以放后端原生包（`.ipk` / `.apk`），它们不是 Pica 归档，只是随包分发给后端安装的载荷。
- `pkgmgr` 是**包身份**的一部分：`opkg`、`apk`、`pica` 三种后端各自独立打包、独立索引、独立安装。
- `opkg` / `apk` 把 `pkgs/` 交给系统包管理器安装；`pica` 的产物只有 `scripts/` 与 `root/`，不接受任何容器。
- Pica 仅负责包分发、校验、生命周期脚本执行，不提供 Docker 管理服务。

## 非目标

明确不做的事，免得把“还没做”和“不该做”混为一谈：

- **不写回 `opkg`/`apk` 的已安装数据库**：容器交给后端安装，装了什么、依赖是谁都由后端的数据库记账。Pica 的账本只记自己铺下的 `root/` 文件与它执行过的脚本，不替后端登记文件。
- **不做依赖求解**：`app`/`base`/`kmod` 一律透传给后端（`opkg install` / `apk add`），由后端的仓库、版本与冲突规则决定装什么。Pica 只决定“走仓库还是走本地容器”和“一次交给后端多少目标”。
- **不解析后端容器格式**：`.ipk`/`.apk` 不打开来安装、记账或求解依赖，格式对委托路径是透明的。唯一读它们的地方是 `pica-pack` 统计 `size`；读不出来（apk-tools 3.x 的 ADB）时只记为未计量并告警。

## 包格式（.pkg.tar.gz）

### 文件名

```
<pkgname>-<pkgver>-<pkgrel>-<platform>-<arch><suffix>.pkg.tar.gz
```

当 `platform = all` 时，为避免出现 `all-all`，文件名简化为：

```
<pkgname>-<pkgver>-<pkgrel>-<arch><suffix>.pkg.tar.gz
```

`<suffix>` 只由 `pkgmgr` 单向生成，永不反向解析：

| `pkgmgr` | `<suffix>` |
| --- | --- |
| `opkg` | （空） |
| `apk` | `-apk` |
| `pica` | `-pica` |

说明：

- 文件名仍以 `platform` 作为 OpenWrt 目标代称。
- `arch` 同样保留在 `manifest` 中，用于区分 CPU/ABI 适配（例如 aarch64/a53）。

### 包内结构

压缩包根目录结构约定如下：

- `manifest`（必需，文件）
- `scripts/`（可选，目录）
- `root/`（可选，目录）
- `pkgs/`（可选，目录；仅 `opkg` / `apk` 后端会包含）
- `LICENSE`（可选，文件）

语义：

- `scripts/`：生命周期脚本，**不进 PATH**。安装时复制到 `/usr/lib/pica/scripts/<pkgname>/`，`scripts/.env` 复制到 `/etc/pica/env.d/<pkgname>.env`。
- `root/`：按目标路径镜像的载荷。`root/etc/init.d/demo` → `/etc/init.d/demo`，`root/usr/bin/demo` → `/usr/bin/demo`。
- `pkgs/`：要交给系统包管理器安装的原生包，打包时按后端过滤后拍平存放（见下）。`pica` 后端没有这一项。

`scripts/` 中约定的文件名与 `manifest` 的对应关系：

| 文件 | `manifest` 字段 | 执行时机 |
| --- | --- | --- |
| `scripts/install` | `script_install` | 首次安装时执行 |
| `scripts/update` | `script_update` | 覆盖安装/升级时执行 |
| `scripts/remove` | `script_remove` | 卸载时执行 |
| `scripts/.env` | — | 提供默认 `PICA_KEEP_SCRIPT_*` 策略 |

`root/` 内的路径必须是相对路径，且不得包含 `..`、不得以 `/` 开头；安装时若发现逃逸路径会直接拒绝该包。

`pkgs/` 的源目录结构按后端和变体分：

```
pkgs/<backend>/<platform>/<arch>/*.<ipk|apk>
pkgs/<backend>/*.<ipk|apk>          # 无变体时的简化写法
```

`<backend>` 只能是 `opkg` 或 `apk`；`pkgs/pica/` 不是输入，见下节。

打包器会把每个 `<platform>/<arch>` 组合单独生成一个 pica 包，并只保留该组合的 `pkgs/<backend>/` 内容；`scripts/` 与 `root/` 会原样打入每个变体包。

### `pica` 后端：只有 `scripts/` 与 `root/`

`targets = pica` 时不接受容器。`pkgdir` 下的 `pkgs/pica/` 会让构建直接失败，报错给出两条出路：改用 `targets = opkg` / `targets = apk` 让设备端的包管理器安装，或在 CI 里先把容器抽成 `root/` 再重新打包。

构建期不解包后端容器的理由：

- **数据库归属**：容器里的文件本来由 `opkg`/`apk` 的数据库记账。把它们摊进 `root/`，同一个路径就有了两个主人——后端的已安装数据库和 Pica 的账本——卸载一边、升级另一边都会留下说不清的状态。
- **依赖求解**：容器的依赖由 `opkg`/`apk` 求解。展开之后只剩下文件，依赖清单不再有执行者，装上去的东西是否完整取决于构建机当时的环境。
- **格式**：`apk-tools` 3.x 的 ADB 容器不是 tar，Pica 不打算为展开而维护 `.ipk`/`.apk` 解析器；委托路径（把 `pkgs/<backend>/` 交给系统包管理器按路径安装）本来就不需要 Pica 理解容器格式，格式对它是透明的。

所以 `pica` 产物的载荷只有 `root/`（加上不进 PATH 的 `scripts/`），`size` 就是 `root/` 里普通文件的总字节数。

`root/` 的装配规则：

- **符号链接原样保留**：打包阶段不跟随链接递归复制，产物里是链接本身，而不是它在构建机上指向的文件。
- **链接目标必须落在 `root/` 内**：绝对路径或 `..` 爬出都直接构建失败。这条规则与设备侧安装期用的是同一个判定（`pica_pkg_core::io::symlink_escapes_root`），因此构建期接受的包不会在安装期因为同一个链接被拒。
- **只有文件、目录与符号链接**：套接字、fifo、设备节点会让构建失败——设备侧只铺这些类型的条目。
- **空 `root/` 也是 `root/`**：显式建一个空目录的包，产物里同样有 `root/`。

### Pica 不解析后端容器格式

Pica 不打开 `.ipk`/`.apk` 来安装、记账或求解依赖：容器一律交给对应后端，由后端的数据库和求解器负责其中的文件。唯一读容器的地方是 `pica-pack` 统计 `size`——按 tar 读 `.ipk` 的 `data` 成员、以及 legacy `.apk`（apk-tools 2.x 的三段 gzip）里的载荷，数完即丢弃，产物里不留痕迹。

`apk-tools` 3.x 的 ADB 容器读不出来（签名块不是 gzip，tar 打不开）。此时构建照常成功、容器照常打进产物，只在构建输出里给出 `WARNING: not counted, pica cannot unpack them: <文件> (<原因>)`，`size` 只统计能统计的部分——宁可比真实值小并且说明，也不静默报 0。

### `apk`/`opkg` 后端：本地容器的安装参数与信任边界

`pkgmgr = apk|opkg` 时，`pkgs/` 里的容器由系统包管理器安装。Pica 只把**归档解包后落在临时目录里的容器文件路径**交给后端，自己不打开它；“按名字从仓库安装”是另一种请求，两者不加同一个参数：

| 请求 | 命令形态 | 信任来源 |
| --- | --- | --- |
| Pica 归档里的本地容器 | `apk add --allow-untrusted [--force-non-repository] -- <path>` | Pica 对归档自身的校验（`repo` 渠道安装时先比对 `repo.json` 的 `sha256`）；`--pkgmgr`/`-U` 手工指定的文件由操作者自己负责 |
| 仓库包（`app/base/kmod` 清单） | `apk add -- <name>` | apk 自己的仓库与签名机制，Pica 不做任何放宽 |

- **`--allow-untrusted`**：豁免 apk 对包签名的检查。Pica 归档里的 `.apk` 不一定由本机信任的密钥签名，所以要装就得放宽；请把它理解为“信任的是这个归档，而不是这个 `.apk` 的签名”。仓库包（按名字安装）永远不会带上这个参数。
- **`--force-non-repository`**：apk-tools 3.x（OpenWrt 25.12 及以后）会拒绝安装不属于任何已配置仓库的软件包，本地文件因此需要显式允许；2.x 没有这个选项，传入未知选项会让命令直接失败，所以 Pica 先读 `apk --version` 的大版本号：**≥3 才加，2.x 不加，读不出来时按需要加处理**（读不出来时更可能遇上 3.x 的拒绝，而多传参数会以 `unrecognized option` 明确报错并提示改配置）。
- 配置开关 `apk_force_non_repository`（默认 `true`）决定是否使用该参数；设为 `false` 时本地 apk 按 2.x 的形态安装。非布尔值（如 `"yes"`）会让命令直接报错，不会替你选一个值——这个开关决定的是“是否放宽一项检查”，拼写错误不该静默生效。
- **`size` 是“装到设备上占多少字节”，不是“归档文件有多大”**：`apk`/`opkg` 后端把 `pkgs/` 整个交给系统包管理器安装，所以 `size` 是 `root/` 加上 `pkgs/` 里**每个容器解包后**的载荷之和。只计普通文件，符号链接与目录不计；容器之间重复提供同一路径会各计一次（这类产物在设备上本来就由后端决定谁胜出）。`pica` 后端没有容器，`size` 就是 `root/` 的总字节数，见上节。

### `pkgs/` 是整包一次调度，不是按阶段调度

`pkgs/` 属于**产物**，不属于 `base` 或 `app` 阶段：打包只按后端过滤它，产物里没有任何信息说明某个容器服务于哪个阶段。因此安装时的规则是：

- **依赖来源只判定一次**：把 manifest 声明的 `base` + `app` 合成一次请求，交给 `--feed-policy` 判定走仓库还是走本地容器。判定结果对两个阶段一致，同一个 `pkgs/` 不会被调度两次。列表为空但带了 `pkgs/` 时按“只能走容器”处理。
- **走仓库时仍分阶段**：`base` 一个请求、`app` 一个请求，先 base 后 app，让 base 依赖先落盘。每个请求内部仍是**一次**包管理器调用，所以同阶段内两个包互相依赖也能被解析。
- **走容器时整批一次**：`apk add --allow-untrusted [...] -- <f1> <f2> ...` 把所有容器放进**同一条命令**。容器之间会互相依赖（应用容器依赖同批的核心容器），而包管理器只在**同一次请求的文件之间**解析依赖；逐个安装会让循环顺序（字典序或任何 Pica 选的顺序）决定安装能否成功，而任何 Pica 能选的顺序都不是依赖拓扑序。
- **kmod 始终走仓库**，并且只装本机还没有的那些（一次请求）。
- `pkgs/` 存在但**一个包都没有**时，视为空包：在安装前的校验阶段就拒绝，不会先装一半再报错。

## pica-pack 输出目录约定

默认情况下（不传 `--outdir`），`pica-pack` 输出到 `pica-pack/bin/<pkgname>/<backend>/`：

```
pica-pack/bin/<pkgname>/opkg/<pkgname>-<pkgver>-<pkgrel>-<arch>.pkg.tar.gz
pica-pack/bin/<pkgname>/apk/<pkgname>-<pkgver>-<pkgrel>-<arch>-apk.pkg.tar.gz
```

每个后端都有自己的子目录。后端由目录表达，而不是从扩展名推断。

## repo.json 强约束（schema + filename）

`pica -S` 同步阶段会对 `repo.json` 做严格校验：

- `schema` 必须为 `2`
- `packages` 必须为数组
- 每个 package 项必须包含非空字符串字段：
  - `pkgname`
  - `pkgver`
  - `pkgrel`
  - `pkgmgr`（`opkg` / `apk` / `pica`）
  - `os`
  - `platform`
  - `arch`
  - `filename`
  - `sha256`
- `size` 必须是数字，表示安装到设备上的普通文件总字节数（`pica-pack` 构建时统计：`pica` 后端为 `root/` 的载荷，`apk`/`opkg` 后端为 `root/` 加上容器解包后的载荷；算不出来的容器不计入并告警）
- `csize` 必须是数字，表示归档文件的压缩后字节数
- `sha256` 必须是 64 位十六进制字符串
- `filename` 必须是纯文件名（不能含 `/`、不能含 `..`），且必须以 `.pkg.tar.gz` 结尾
- `filename` 必须与其他字段一致：
  - `platform != all`：`<pkgname>-<pkgver>-<pkgrel>-<platform>-<arch><suffix>.pkg.tar.gz`
  - `platform = all`：`<pkgname>-<pkgver>-<pkgrel>-<arch><suffix>.pkg.tar.gz`
  - `<suffix>` 见上文文件名表
- 未识别的额外字段会被原样保留（`flatten`），不会在同步时丢失

安装阶段（`-S <selector>`/`-Sp`）在下载后、写入缓存和安装前，必须校验下载文件 SHA-256 与 `repo.json` 的 `sha256` 一致，否则失败。
- 可选 `download_url`（若提供）必须是 `http://`、`https://` 或 `file://`，用于覆盖默认下载路径

同后端同名包在同一索引中只允许出现一次；`opkg`、`apk`、`pica` 三种后端的同名包可以并存，因为 `pkgmgr` 参与包身份。

任一约束不满足，`pica -S` 会拒绝该 repo。

## URL 安装

`pica -U` 支持本地文件和 URL：

```
pica -U ./hello-0.2.8-1-all.pkg.tar.gz
pica -U https://example.invalid/pkgs/hello-0.2.8-1-all.pkg.tar.gz
```

允许的 URL 协议：

- `http://`
- `https://`
- `file://`

## 安装数据库（db.json）字段语义

`/var/lib/pica/db.json`（可用 `PICA_STATE_DIR` 重定向）记录已安装项。每个条目的字段回答不同的问题，不能互相替代：

- `source`：**这个包是怎么被 Pica 拿到的**，取值 `pica` / `local` / `url` / `feed`。
- `pkgmgr`：**哪个后端真正拥有这些文件**，取值 `opkg` / `apk` / `pica`。
- `pkgfile`：产物安装时是归档路径；`feed` 安装时是后端名（因为不存在 Pica 归档）。

写入规则：

- `source` 与 `pkgmgr` 在安装时**覆盖写入**，不沿用产物 manifest 里的值：
  - `--pkgmgr pica -U x.pkg.tar.gz` 会记住 `pkgmgr = pica`，即使归档声明的是 `apk`，卸载时也就不会去驱动 apk。
  - URL 下载会记住 `source = url`，即使打包时写的是 `pica`。
- 因此 `source` 描述渠道，`pkgmgr` 描述后端；二者取值空间不重叠（历史遗留的 `source = opkg` 除外，见下）。

读取规则（旧记录迁移，无需改写数据库）：

- 后端解析阶梯：`pkgmgr` → 旧 `source` 中的后端名 → 旧 `pkgfile` 中的后端名 → `pkgfile` 是归档路径时取 `pica` → 兜底 `opkg`。
- 历史记录里 `pkgmgr` 写作 `none` 的行仍可读出，会被当作 `pica`；但用户输入的 `--pkgmgr none` 是错误，不再接受。
- 渠道解析：`source` 为可识别渠道时直接用；否则按 `pkgfile` 是否为归档路径判定 `pica` 或 `feed`。
- 第 2~5 档属于推断，`pica -R` 会打出 warning 说明后端来源；`-Qi` 展示的是归一化后的渠道与后端。
- `-Qi` 只打印**有值**的字段：某字段在这条记录里为空（旧记录没有 `pkgver`、包本身没有 `license`）就整行省略，不打出“标签 + 空值”。空标签比省略更少信息，还会让人以为值曾经存在。`Name` 是唯一的例外——它是这条记录的标识，行行都有。字段取值的列宽固定为 16 字符，省略某行不会让下面的行错位。

## JSON 输出（显式开启）

`pica` 默认输出面向人工的文本日志，不改变现有使用习惯。

可选参数：

- `--json`：启用 JSON 输出（成功/失败）
- `--json-errors`：仅失败时输出 JSON

## 非交互与来源策略

可选参数：

- `--non-interactive`：禁用交互提示，适配后端调用
- `--feed-policy <mode>`：来源策略（对整包判定**一次**：`base` + `app` 合并成一次请求，见「`pkgs/` 是整包一次调度，不是按阶段调度」）
  - `ask`（默认）
  - `feed-first`
  - `packaged-first`
  - `feed-only`
  - `packaged-only`
- `--with-optional` / `--no-optional`：全装 / 全不装可选依赖（`?`），默认在终端上询问、非终端一个不装（见「可选依赖（`?`）」）。两者互斥。

## 依赖索引：答不出来就不答

pica 自己回答"某个包在不在本机配置的源里"时（`-S` 的来源选择、`-U` 的依赖预检、`-S` 之后的依赖检查），答案只来自本机后端自己的索引：`opkg` 是每个 feed 的列表文件（`lists_dir` 下的同名文件），`apk` 是每个仓库的索引（本地仓库就地读取，网络仓库按索引 URL 的摘要缓存在缓存目录）。

- 查询前先保证索引可读：只有确实缺索引才调用一次 `opkg update` / `apk update`；已有索引不重复拉取，这是整条路径上唯一昂贵的步骤。
- 缺索引且刷新没能补上（断网、源地址失效、`opkg` 锁被活进程持有）报 `E_INDEX_UNAVAILABLE`，消息里带上缺哪些源、刷新命令的退出码与其输出，**不会**把"取不到索引"当成"源里没有这个包"，也不会因此把依赖报成 missing。
- "本机的源"按后端自己读配置的方式认定：`opkg` 是 `/etc/opkg.conf` 与 `/etc/opkg/*.conf`，`apk` 是 `/etc/apk/repositories` 加 `/etc/apk/repositories.d`、`/lib/apk/repositories.d` 下的 `*.list`（OpenWrt 25.12 的 feeds 就在那里，`/etc/apk/repositories` 是空的）。一个源都没配置就是"本机没有源"，不是错误。
- `--feed-policy packaged-only` 不询问源，因此在源不可达的主机上依旧可以只从 pica 仓库安装。

## app 安装顺序

`app` 阶段固定分组顺序：

1. 主程序（core）
2. `luci-app-*`
3. `luci-i18n-*`

若某组为空则直接跳过（例如纯 CLI 包没有 luci/i18n）。

说明：

- 这些参数为显式开启；不传时行为保持不变。
- 当某些命令本身已经输出文本结果（例如 `-Q/-Qi/-Ql`），成功 JSON 会被自动抑制，避免混淆机器解析。
- `--json/--json-errors` 由 Rust 侧直接输出 JSON，不依赖任何外部 JSON 工具。

## 兼容维度：os + arch + uname（platform 仅展示）

为了让安装/更新行为尽可能稳定，pica 的“实际兼容性判断”使用：

- `os`：系统类型（例如 `openwrt`），强约束

- `uname`：与 `uname -m` 严格匹配（跨系统最通用的基线）
- `arch`：OpenWrt/opkg 定义的架构字段（可通过 `opkg print-architecture` 查看），推荐统一使用 `all`

`platform` 仍然保留，但它只用于应用商店/仓库展示与筛选，不作为安装的硬性门槛。

在需要展示时（日志、查询）应同时展示 `os + platform + arch + uname`（若 `uname` 未提供则可省略）。

## manifest（Arch-like 文本）

### 格式

- `key = value` 一行一个字段
- 支持 `#` 注释
- 支持重复 key（表示数组）
- 支持后端覆盖 key：`<key>@<backend>`，`<backend>` 取 `opkg` / `apk` / `pica` 之一；该键存在时，这个后端只用它、不再看 `<key>`（整体覆盖，见“与安装相关的扩展字段”）

### 字段类型规则

同一个字段名不允许在构建期与运行期有两种含义，因此：

- `targets`（可重复）：只出现在**源码 manifest**，声明要构建哪些后端，默认 `opkg`。
- `pkgmgr`（单值）：只出现在**产物 manifest**，由 `pica-pack` 写入，声明这个包属于哪个后端。

源码 manifest 声明 `pkgmgr` 会让 `pica-pack` 直接报错（应改用 `targets`）；`targets` 属于构建期字段，会在打包时从产物 manifest 中剥离，安装期不再出现。

### 必需字段

```
pkgname = <name>
appname = <name>
pkgver = <version>
os = <system>
```

以下字段有默认值，可省略：

- `pkgrel`：省略时，`pkgver = 1.0.0-2` 会拆出 `pkgrel = 2`，否则默认 `1`
- `platform`：默认 `all`
- `arch`：默认 `all`
- `pica`：省略时不校验最低 pica-cli 版本
- `targets`：默认 `opkg`

### 推荐模板

```ini
# Required
pkgname = hello
appname = hello
pkgver = 0.2.8
os = openwrt

# Optional build targets (repeatable, default: opkg)
targets = opkg
targets = apk
# targets = pica

pkgrel = 1
platform = all
arch = all
pica = 0.2.8

# Optional metadata
pkgdesc = Example lifecycle package
packager = pica-pack
license = GPL-3.0-only
visibility = open
url = https://github.com/miaoermua/pica
luci_url = https://github.com/openwrt/luci/tree/master/applications/luci-app-hello
branch = stable
protocol = luci
luci_desc = LuCI plugin for hello service
source = pica

# Optional strong compatibility gate
# uname = aarch64

# Optional tags
# type = cli
# type = luci
# luci = lua1

# Install plan (repeatable, optional per-backend override)
app = hello
app_i18n = luci-i18n-hello-{lang}
base = busybox
# A bound is checked by pica, never passed to the backend (see below)
# base = luci-base>=1.2
# A trailing ? is optional: offered to the user, never a reason to fail
# app = luci-app-hello-extra?
# kmod = kmod-tun
# 两个后端名字不同时才需要这一行；名字通常相同。
# base@apk = <该依赖在 apk 源里的名字>

# Lifecycle scripts (optional, paths inside scripts/)
script_install = scripts/install
script_update = scripts/update
script_remove = scripts/remove
```

关于 `pica` 字段：

- 表示“最低兼容 pica-cli 版本”（minimum required），不是“必须完全一致的版本”。
- 不写 `pica`：默认不做版本门槛校验（为了兼容旧包）。
- 写了 `pica` 且本机 `pica-cli` 版本低于该值：安装/更新会失败并提示无法兼容。

可选字段（强兼容校验，优先判断）：

```
uname = <uname>
```

### 可选字段（建议）

```
pkgdesc = ...
url = ...
luci_url = ...
packager = ...
builddate = <unix timestamp>
size = <bytes>

# license metadata
license = GPL-3.0-only
visibility = open
```

约定：

- `builddate`：可选；Unix 时间戳（秒）。推荐不要在“源码 manifest”里手写，由 `pica-pack build` 在构建产物中自动补全。
- `size`：可选；字节数（bytes），口径见「`pica` 后端」与「`apk`/`opkg` 后端」两节（装到设备上的普通文件总字节数：`pica` 为 `root/`，其余后端再加上容器解包后的载荷）。推荐不要在“源码 manifest”里手写，由 `pica-pack build` 在构建产物中自动计算并补全。

校验文件（由 `pica-pack build` 自动生成，不写入 manifest）：

- `SHA256SUMS`：输出目录下的校验清单，格式为 `<sha256>  <filename>`
- 每次构建会为产物 `.pkg.tar.gz` 写入/更新对应条目

### 与安装相关的扩展字段（可重复，可选）

```
# Install plan (repeatable)
app = <native package name>
base = <native package name>
kmod = <native package name>
# A trailing ? marks an optional dependency (see 「可选依赖（?）」)
app = <native package name>?

# Backend override (repeatable): <key>@<backend>, backend = opkg | apk | pica
# The annotated key replaces the plain key wholesale for that backend.
# All three backends follow the same rule; only the name after `@` changes.
app@apk = <该依赖在 apk 源里的名字>
base@opkg = <该依赖在 opkg 源里的名字>

# Optional: i18n template for app packages
# The {lang} placeholder is resolved from pica config (default: zh-cn).
app_i18n = <native package name template>

# Program source metadata
url = <project homepage or repository URL>

# Optional LuCI source metadata
luci_url = <LuCI plugin homepage or repository URL>

# LuCI description metadata (optional)
luci_desc = <short LuCI plugin description>

# lifecycle scripts (optional)
script_install = <relative file>
script_update = <relative file>
script_remove = <relative file>

# optional type tags (repeatable)
type = cli
type = luci

# when type includes luci
luci = lua1
```

约定：

- `app/base/kmod` 定义“安装清单”，pica 安装时遍历这些字段决定需要安装的原生包。
- `<key>@<backend>` 是该后端的**整体覆盖**，不是追加：只要出现了 `<key>@<backend>`，该后端就只用带注解的那些值，未带注解的 `<key>` 全部忽略；其他后端仍用 `<key>`。`<backend>` 取 `opkg` / `apk` / `pica` 之一，三者规则相同——解析器只把名字拼成 `{key}@{backend}` 去查，不校验那个名字，而查的时候用的永远是**当前产物自己的** `pkgmgr`。
  - 两个后端名字不同时：`base = <依赖名>` + `base@apk = <该依赖在 apk 源里的名字>`。名字通常相同，只有确实不同才需要写这一行。
  - 只有 opkg 需要它时：只写 `base@opkg = <该依赖在 opkg 源里的名字>`，不写裸 `base`（见下面的选型指导）。
- `<key>@<backend> =`（空值）表示该后端这组一个都不装。空值之所以有意义，是因为读取侧把它当成“没有依赖”而不是“一个名字为空的依赖”：`get_array_for` 对存在但为空的键返回 `[""]`，而 `constraint::parse_all` 会跳过空项——跳过这一步，空值就成了非法声明。
- 解析顺序为 `<key>@<backend>` → `<key>` → 空。
- 选型：**只有某个后端该装这条依赖** → 不写裸 key，只写 `<key>@<那个后端>`；**某后端不该装、其他后端照旧** → 写裸 key，再加 `<key>@<那个后端> =`（空）遮蔽。
- 这两种写法都能在 `targets` 含 `pica` 时通过构建检查：`pica-pack` 只读 `get_array_for(key, "pica")`。前者没有裸 key 可回落，得到空列表；后者读到空值，被空项过滤掉。两者都不会触发“pica 不能声明依赖”的报错。
- `app_i18n` 允许包含 `{lang}` 占位符，pica 根据配置 `i18n` 选择实际 i18n 包名（当前仅 `zh-cn` 时参与安装/卸载）。
- `url` 用于记录程序来源（建议直接填仓库/项目地址，如 GitHub URL）。
- `luci_url` 用于记录 LuCI 插件来源（可选；仅 LuCI 包建议填写）。
- `luci_desc` 用于记录 LuCI 插件描述（与 `pkgdesc` 的通用程序描述分离）。
- `pkgmgr = opkg|apk` 时：按 `app/base/kmod` 安装，按 `app` + `app_i18n`（`i18n=zh-cn`）卸载，`pkgs/` 中的原生包交给系统包管理器。
- `pkgmgr = pica` 时：Pica 部署 `root/`、执行生命周期脚本，不解析依赖。产物里不该有 `pkgs/`——带了的归档在写入任何文件之前就被拒绝（`E_PACKAGE_INVALID`），因为 pica 不读容器格式，既装不了它，也无法为里面的文件记账。
- `root/` 按路径镜像部署到真实根文件系统，安装的文件与目录会被记录，卸载时逐条回滚。
- `script_install/script_update/script_remove` 是生命周期脚本（包内路径，一般在 `scripts/` 下），应按 POSIX `sh` 语法编写。
- 生命周期脚本由系统 `sh` 执行（OpenWrt 默认 BusyBox `sh`），不依赖 `bash`。
- 生命周期脚本保留策略（默认）：`script_remove` 保留；`script_install/script_update` 执行后不保留。
- 如果包内无任何生命周期脚本，视为无生命周期包：安装/升级/卸载均不会触发脚本执行。
- 载荷至少提供其一，否则构建与安装都会以“空包”拒绝：对每个后端而言，`scripts/` 与 `root/` 都算；`pkgs/<backend>/` 只对 `opkg`/`apk` 算。所以一个只放 `pkgs/opkg/` 的 `pkgdir` 能产出 opkg 包，但同一份 `targets` 里的 `pica` 产物会因为没有任何文件而被拒绝。
- 可在 `scripts/.env` 中声明保留策略：`PICA_KEEP_SCRIPT_INSTALL/PICA_KEEP_SCRIPT_UPDATE/PICA_KEEP_SCRIPT_REMOVE/PICA_KEEP_SCRIPT_ALL`（布尔值）。
- 每次命令事务结束都会打印 `正在清理 pica 缓存`，并清理 `/var/lib/pica/cache/pkgs/` 目录内容。

- `type` 允许声明应用形态标签，便于 pica 在安装阶段做额外兼容检查。
- `type = luci` 表示“该包包含/依赖 LuCI Web UI”。如果声明了 `type = luci`，必须同时声明 `luci = lua1|js2`。
- `type = cli` 表示“该包提供纯命令行程序/脚本”。
- 仅声明 `type = cli` 时，不需要 `luci = ...`。
- `source` 用于记录安装来源（例如 `pica` / `opkg` / `local` / `url`），便于升级与排错。默认可不写；安装后 CLI 会在本地数据库里补全 `source`。

### 依赖版本约束（`app` / `base` / `kmod`）

每一项都可以带一个版本约束：

```
base = luci-base>=1.2
app  = luci-app-demo>1.0
kmod = kmod-tun=0.1
```

支持六种写法：`name`、`name>=ver`、`name<=ver`、`name>ver`、`name<ver`、`name=ver`。比较复用 `pkgver_cmp`：按最后一个 `-` 切成 `pkgver` 与 `pkgrel` 两段，先比 `pkgver`，相等再比 `pkgrel`。`pkgrel` 不要求是纯数字，用与 `pkgver` 相同的分段规则（数字段按数值、字母段按名字、同位置的数字段高于字母段），所以 opkg 的 `-1` 和 Alpine 的 `-rN` 都按修订号比较：`1.2-1`、`1.2-r1` 都高于 `1.2`，`1.2-r2` 高于 `1.2-r1`；不写 `pkgrel` 与 `-0` 等价（两边都把缺失的修订号当 0）。写 `=1.2` 要求两段都相等，所以 `1.2` 不满足 `=1.2-1`。全角 `≥ ≤ ＞ ＜ ＝` 按对应的 ASCII 运算符处理，其余形状（`>>`、`==`、一个声明里两个运算符、缺名字或缺版本）在解析阶段就报 `E_MANIFEST_INVALID`。

包名只允许 `[A-Za-z0-9._+-]`，其余字符（如 `libfoo!=1.2` 里的 `!`）同样在解析阶段就报 `E_MANIFEST_INVALID`，不会拖到预检才变成一条"缺失依赖 `libfoo!`"。

- **约束只用于 pica 侧校验，不传递给后端命令行。** opkg 只按包名或 `.ipk` 路径安装，apk 把 `name>=ver` 当作一个原子；同一个字符串交给两者，必然一边成功一边报错。pica 传给后端的永远是裸包名。
- **pica 没有预发布语义表**，不要用 `-rc` / `-beta` 表达"低于正式版"：`1.0.0-rc1` 会被切成 `pkgver=1.0.0` + `pkgrel=rc1`，因而**高于** `1.0.0`。需要这种关系时，请改用容器或调整 `pkgver`。
- **需要精确版本请用 `pkgs/<backend>/` 携带容器**，由后端按路径安装：版本由容器自身决定，pica 不表达、也不校验它。
- 校验时机：
  - 安装预检（`-U` / `-S`）：依赖**已安装**且版本不满足时报错（`E_VERSION_INCOMPATIBLE`），消息里同时给出实际版本与要求的约束；依赖**尚未安装**时不阻塞安装，约束写进 `install-report` 供审计。已安装且带约束时，预检条目还会写 `bound_satisfied`：键缺席表示这次没有可比较的版本（没装、或没写约束），与 `false` 不是一回事。
  - 安装第 5 步（交给后端）之后：同一组约束再判一次，不满足只 `warning`——容器已经落盘，pica 不撤销后端选定的版本。
  - 升级（`-Su`）：后端升级完成后复查所有已安装包声明的约束，不满足逐条 `warning`，不自动降级。
- 约束不会让源里没有的包变成有的：缺失依赖仍走原有的 precheck 报错。
- `-Qi` 的 `Requires` 字段按原文列出带约束的声明（如 `base:luci-base>=1.2`）；不带约束的包名仍由 `Depends` 汇报。

### 可选依赖（`?`）

在声明末尾加 `?` 表示"可选，不是必需"：

```
app  = luci-app-mosdns?
kmod = kmod-tun>=0.1?
```

- `?` 必须在整条声明最后，版本约束写在它前面（`luci-app-demo>1.0?`）。`?` 不是包名字符，因此不会与真实包名混淆；`??` 会在名字校验里被拒，而不是被读成"更可选"。
- 同一个包名同时以必需和可选出现时，必需的那条生效：`resolve` 只留一条，后端与报告都看不到第二份。
- 可选项与必需依赖走同一套预检（仍会问源里有没有、本机装没装），区别只在缺失或版本不符时**不**报错，状态照常写进报告。

装不装由 `--with-optional` / `--no-optional` 决定，默认是"能问就问"：

| 情况 | 行为 |
| --- | --- |
| 默认、stdin 是终端 | 列出可选项，按编号、`all` 或回车跳过 |
| 默认、stdin 不是终端或 `--non-interactive` | 一个不装；不是终端时在 stderr 说明可以用 `--with-optional` |
| `--with-optional` | 所有可装的可选项都装，不询问、不需要终端 |
| `--no-optional` | 一个不装，也不输出上一条提示 |
| 两个一起给 | `E_ARG_INVALID`，不猜哪个优先 |

- "可装"按来源判定：`feed-policy` 选了 `pkgs/` 时，源里的可选包不算可装（会和容器重复安装），标为 unavailable；`kmod` 例外，始终走源。`pkgmgr=pica` 没有解析器：必需依赖仍直接拒绝整包，可选依赖只 warning 并跳过。
- 已安装的可选项只展示、不重装；报告中的 `outcome` 区分 `installed`（本次装入）、`installed_before`（本来就有）、`declined`（没选）、`unavailable`（装不了）、`installed_with_pkgs`（随 `pkgs/` 一起装）。已装但版本不满足声明的约束时，`outcome` 仍是 `installed_before`（这次安装确实不会动它），但选择器会写明"已装版本不满足声明的约束（约束原文）"，报告条目带 `reason`——"装着的版本不对"与"装不上"是两件事。
- 选择器里的两个尺寸都来自后端自己的源索引，且只描述**这个包本身**、不含它的依赖：`Download` 是下载的包文件大小（opkg 列表的 `Size`、apk 的 `file-size`），`Installed` 是解包后的大小（`Installed-Size` / `installed-size`）。取的是后端真正会装的那个版本（裸包名 = 源里最高版本），两个数字互不推导：源里没写、或写成 `0`（apk 的 ADB 容器就是这样），就显示 `Unknown`。已安装与随 `pkgs/` 一起装的可选项没有下载动作，不显示尺寸。
- 报告写在 `install-report` 的 `precheck.optional`（数组，每项含 `group` / `name` / `optional` / `outcome`，带约束时含 `constraint`，已装版本不满足约束时含 `reason`（原始措辞与 stderr 上那句一致），源里报了尺寸时含 `download_size` / `installed_size`，单位字节）。它是 `precheck` 里的新增键，旧的读取方按原有 `kmod` / `base` / `app` 字段照常工作。
- 选择器与提示一律走 stderr，stdout 只留命令结果，`--json` 不会被污染。

## arch（后端架构字段）

`arch` 不是 pica 自定义标签，它对应后端的架构字段。

推荐实践：

- 对绝大多数 pica 包：统一写 `arch = all`
- 仅当确实需要限制某些设备时：写具体 arch（例如 `aarch64_cortex-a53`）

`pica-cli` 的检查逻辑：

- `arch = all`：永远允许
- `arch != all`：必须出现在当前后端的架构列表中，否则安装失败
  - `opkg`：取自 `opkg print-architecture`
  - `apk`：取自 `apk --print-arch`

## os（系统类型）

`os` 用于标识系统类型（例如 `openwrt`）。

`pica-cli` 的检查逻辑：

- `os = all`：永远允许
- `os != all`：必须与当前系统类型一致，否则安装失败

## 一个 app 多个原生包

无论 opkg 还是 apk 生态，一个“应用”（你希望用一个 `pkgname` 表示）通常拆成多个原生包：

```
myapp                 # 本体二进制/服务
luci-app-myapp        # LuCI 插件
luci-i18n-myapp-zh-cn # i18n
```

因此建议：

- `pkgname` 用“应用名”做 pica 包唯一标识。
- 安装清单用 `app = ...`（以及可选 `app_i18n = ...`）列出应用子包。
- 两个后端包名不同时，用 `<key>@<backend>` 覆盖，例如 `base = <依赖名>` 配 `base@apk = <该依赖在 apk 源里的名字>`。两个后端名字通常相同，只有确实不同才需要写这一行。
- 卸载时 pica 按 `app` + `app_i18n` 映射处理（当前仅 `i18n=zh-cn` 时处理 i18n 包）。

## 包命名：`pkgname` 与 `appname`

pica 用两个字段承载“名字”，职责不同，不能混用：

- `appname`：纯上游应用名，是命令行的选择键。`Package::app_key()` 取 `appname`（为空时回落到 `pkgname`）；`pica -S <app>` 收集候选时只按 `appname` 匹配，与它来自哪个源、`pkgname` 是什么无关。
- `pkgname`：这个包在 pica 源里的唯一身份，原样写进产物文件名（`pica-pack` 只要求它非空，不校验字符集）。区分“谁做的 / 什么形态”的后缀只加在 `pkgname` 上。

按“谁做的、做成什么形态”选择 `pkgname`：

| 情况 | `pkgname` | `appname` |
| --- | --- | --- |
| 上游官方支持 OpenWrt / LuCI | `demo` | `demo` |
| OpenWrt 官方社区打包 | `demo` | `demo` |
| 只有二进制可用（上游没有现成的 OpenWrt 打包，pica 只是把可执行文件铺上去） | `demo-bin` | `demo` |
| 第三方个人做的 | `demo@<用户名>` | `demo` |
| 第三方社区 / 组织做的 | `demo@<组织名>` | `demo` |

约定：

- **去掉 `luci-app-` 前缀**：LuCI 包的真实身份是它服务的那个应用，而不是 Web 界面本身。`pkgname`/`appname` 都写 `demo`，把原生容器名留在安装清单里（`app = luci-app-demo`），这样用户敲的是 `pica -S demo` 而非 `pica -S luci-app-demo`，安装体验更好。
- **`-bin` / `@<谁>` 只进 `pkgname`，不进 `appname`**：选择器语法里 `@` 是分支分隔符（`pica -S demo@stable` 解析为 appname `demo` + branch `stable`）。把 `@` 放进 `appname` 会让这个包永远无法被名字选中；放在 `pkgname` 里只作身份与展示，不干扰选择。
- **`pkgname` 里的 `@` 不会到后端**：后端只接收 `app` / `base` / `kmod` 里的原生包名，那些名字不允许 `@`（只允许 `[A-Za-z0-9._+-]`，否则 `E_MANIFEST_INVALID`）。`@` 属于 pica 侧身份，从不下发给 `opkg` / `apk`。
- **同一 `appname` 的多个包**（如官方 `demo` 与某个 `demo@alice`）：`pica -S demo` 把二者都纳入候选，取 `pkgver-pkgrel` 最高的那个（版本相同则先出现的源胜出）。`@<谁>` 只用于区分身份与展示，当前不提供“按 `pkgname` 精确选某一方”。
- **语言包**：`app_i18n = <模板>-{lang}` 里的 `{lang}` 由配置项 `i18n` 解析（默认 `zh-cn`）；从源直接安装应用时，pica 会顺带尝试 `luci-i18n-<app>-<lang>`（当前仅 `zh-cn` 参与安装 / 卸载）。

## scripts/.env 预留

`scripts/` 目录下允许存在一个可选文件：

```
scripts/.env
```

用途：为生命周期脚本提供环境变量预留（默认不要求存在）。

约定：

- 安装时复制到 `/etc/pica/env.d/<pkgname>.env`
- 卸载时同步删除该 env 文件
- 可选生命周期保留控制（布尔值）：
  - `PICA_KEEP_SCRIPT_INSTALL`
  - `PICA_KEEP_SCRIPT_UPDATE`
  - `PICA_KEEP_SCRIPT_REMOVE`
  - `PICA_KEEP_SCRIPT_ALL`

## type/luci：LuCI 版本/实现（可选）

OpenWrt 上的 Web UI 可能存在不同实现（例如历史上的 Lua/LuCI1 与 luci2 JS）。

当你的包包含 LuCI 插件或强依赖某种 LuCI 实现时，建议：

- 增加 `type = luci`
- 声明 `luci` 的实现版本

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

- 未包含 `type = luci`：不做 LuCI 兼容检查（`luci = ...` 即使存在也不会触发检查）
- 包含 `type = luci`：必须同时声明 `luci = lua1|js2`
- `pica -U` 会尝试检测本机 LuCI 实现并匹配；无法检测或不匹配则安装失败

## 许可证（license / visibility）

manifest 中允许定义许可证信息：

```
license = GPL-3.0-only
visibility = open
```

约定：

- `license`：建议用 SPDX 标识（例如 `GPL-3.0-only`、`MIT`、`Apache-2.0`）
- `visibility`：软件可见性标签，取值只能是 `open|mix|closed`
- 当前版本只做“定义与展示”，不做任何许可证强制校验

## LICENSE 文件（包内可选）

打包时允许在 staging 根目录放一个 `LICENSE` 文件，`pica-pack` 会把它原样打进压缩包根目录：

```
LICENSE
```

当前版本不自动安装/展开该文件，仅作为后续 `pica` 命令显示许可证内容的基础。

## app 选择器

`pica -S <selector>`/`pica -Sp`/`pica -Si` 支持以下选择器（全角符号也支持）：

```
app
app(branch)
app:branch
```

约定：

- 当前选择器仅支持按 `branch` 过滤。
- 当前不提供历史版本安装，仓库仅保留最新包。

## manifest 示例（LuCI1，双后端）

```ini
pkgname = example
appname = example
url = https://github.com/example/example
luci_url = https://github.com/openwrt/luci/tree/master/applications/luci-app-example
branch = openwrt-23
protocol = luci
luci_desc = Example LuCI plugin for OpenWrt

pkgver = 1.0.0
pkgrel = 1
pkgdesc = Example LuCI application
packager = example
license = GPL-3.0-only
visibility = open

# 要构建的后端（可重复，默认 opkg）
targets = opkg
targets = apk

arch = all
os = openwrt
platform = amd64
uname = x86_64

pica = 0.2.8
source = pica

type = luci
luci = lua1

# Install plan (repeatable)
app = luci-app-example
app_i18n = luci-i18n-example-{lang}
base = luci-base
base = rpcd
base = ca-bundle
# base@apk = <该依赖在 apk 源里的名字>   # 仅当 apk 侧名字不同
kmod = kmod-tun

# Lifecycle scripts (optional)
script_install = scripts/install
script_update = scripts/update
script_remove = scripts/remove
```
