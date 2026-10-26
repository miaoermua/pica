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
  - 安装事务的备份 `/var/lib/pica/cache/backup/<pkgname>-XXXXXX/`（只装这次 walk 即将覆盖的文件副本；目录 `0700`、副本 `0600`，提交后删除，回滚没恢复完时保留并在告警里报出路径。不是 journal：崩溃或换进程后没有谁去读它，见下节）。
- **静态资源部署桩**：
  - `/usr/lib/pica/scripts/<pkgname>/` （执行后视环境变量策略定夺清扫还是滞留的生命周期脚本集）
  - `root/` 内的文件按路径镜像部署到真实根文件系统，卸载时按 `db.json` 记录逐条回滚

### `root/` 普通文件的落盘语义

- **同目录临时文件 + rename**：`root/` 里的普通文件由 `pica_pkg_core::io::deploy_file` 部署——在目标文件的**父目录**里建临时文件（`tempfile::NamedTempFile::new_in(parent)`），写入载荷字节，取归档里的权限位，最后同目录 rename 落位。临时文件不能建在 `env::temp_dir()`：那是解包用的暂存目录，在设备上通常属于另一个文件系统，跨文件系统 rename 会失败。
- **为什么不是"先删再写"**：先 unlink 再复制是为了躲 `ETXTBSY`（正在运行的守护进程其二进制被内核持有），rename 同样躲得过——落位的是新 inode，运行中的进程继续用旧 inode。但先删会开一个窗口：旧内容已经没了、新内容还没写完，这期间失败就销毁了上一版的文件，而那时没有任何副本可回。rename 让目标路径的内容只变一次，失败路径上目标保持原样、临时文件被清掉；至于已经成功落位、之后才被别的步骤判定失败的那些文件，旧内容由下一节的事务备份负责还原。
- **权限位**：`tempfile` 建出来的是 `0600`，rename 带的又是临时文件自己的模式，所以落位前必须显式取归档里的模式，否则可执行文件和 `/etc/init.d/*` 装上去就跑不了。
- **符号链接目标**：目标处已经是符号链接时，由 rename 替换链接本身，不会跟随链接把内容写进它指向的文件；载荷里指向 `root/` 之外的链接仍在写入前被 `symlink_escapes_root` 拒绝（判定见 `standard.md`）。
- **目标处是目录**：空目录照旧删掉再落文件；非空目录不删（它不属于这次 walk），于是 rename 失败、整个安装报错，那个目录和里面的内容都不动。
- **记账**：`install_payload_with` 交回的仍是"这次 walk 落下的全部路径"，其中包含被替换的文件；`deploy_payload` 拿 `intended_payload`（walk 开始前不存在的路径）与它求交，才决定 `created_records` 里放什么，所以被替换的路径不会被记成"本次新建"，回滚也不会去删它。临时文件在 rename 之前不进任何列表；walk 中途失败时它**保留已经落下的那部分**（边写边记，不因为后面出错就作废），登记时只取 `intended_payload` 里**确实已经在盘上**的那些路径（`existing_only`）：没被走到的路径不谎报，被替换的路径也不算"本次新建"。

### 被覆盖文件的备份与事务回滚

rename 落位是"最终"的：一旦改名，旧内容就再也摸不到了。所以旧内容必须在 walk 开始之前离开。

- **备份时机**：`deploy_payload` 在建好 `root/` 的目标路径映射后、复制任何字节之前调用 `prepare_payload_backup`，用与 `collect_intended` 相同的映射遍历 `root/`（`snapshot_payload`），把目标处**已存在**的对象逐个复制进 `cache/backup/`。顺序是全部要点：缓存目录写不了、目标文件读不了、目标处是不可复制的对象（socket、fifo、设备节点），都在这一步失败并直接返回错误——此时设备上的字节一个都还没少，也还没有任何东西可回滚。
- **备份的三种对象**：普通文件（副本 + 原权限位；恢复时同样用 `deploy_file` 一次 rename 落回，所以"撤销一次部署"和"做一次部署"是同一套原语）；符号链接（保留链接本身而不是它指向的内容，恢复时先清掉占位再重建链接）；空目录（walk 只会删掉空目录，恢复时清掉占位再建目录）。非空目录不需要备份：`remove_existing` 删不掉它，它本来就还在。其余类型在写入前拒绝，错误文本 `is neither a file, a symlink nor a directory`。
- **备份不是载荷**：副本不进 `payload_records`、不进 `created_records`、不进 `db.json`、不计入 `Installed <n> files, <bytes>`，`-R` 也删不到它。被替换的路径本身照旧记进安装列表（这次安装确实写了它），所以安装清单、大小统计与卸载行为和备份之前一致。
- **恢复顺序**：`rollback` 先放内容（按备份的倒序，与 `remove_payload` 处理目录的规则一致：先腾位置再重建父目录），再删除本次新建的路径，然后 hook 补偿、后端容器回收、保留脚本还原。恢复失败的路径以 `content:<路径> (<原因>)` 进 `leftovers`，备份目录**保留**（那是那些文件唯一的副本），告警文本给出目录位置。
- **提交后才清扫**：`record_installation` 成功 → `mark_committed` → `finish` 才删备份目录；删不掉就报出路径而不是静默忽略。回滚跑完（内容全部落回）时目录随之清空，第二次回滚是无操作而不是二次部署。
- **硬链接**：目标处是硬链接时，rename 换的是 inode，另一个名字自始至终没被动过；恢复后两个名字内容一致但不再是同一个 inode。这就是"按文件恢复"的含义，备份不做硬链接关系的还原。
- **保证边界**：单个文件的替换是原子的，一次安装的多个文件不是整体原子；这是**进程内**回滚——备份只活在这一次事务里，没有跨进程可续读的 journal。进程被杀或掉电后，设备上的文件与副本都留在原地，下次运行不会自动恢复；那时能依靠的仍然是同目录 rename 本身（目标要么完整是旧内容、要么完整是新内容）。恢复失败、清扫失败都会显式报出路径与原因，不会把部分恢复说成"回滚成功"。
- **不会被清扫的那一份**：每次事务结束的清缓存只扫 `cache/pkgs/`，`cache/backup/` 里的东西没有人再读也没有人再删——进程中途被杀留下的备份目录（以及提交后剩下的空 `cache/backup/` 父目录）会一直躺在缓存里，要释放 Flash 得人工删。这是"不做 journal"这个选择的代价，写在这里而不是藏起来。

### 安装的提交顺序与失败处理

- **顺序**：hook → `root/` 部署 → `scripts/.env` → `record_installation`（先数据库、后安装报告）→ `mark_committed` → `finish`。数据库是"装上了没有"的唯一答案：`-Qi`、`-R`、`-U` 都只读它，报告不是。记账发生在文件已经落盘之后、完成提示之前，所以看到完成提示就意味着 `db.json` 里已经有这条记录。
- **成功才报完成**：`Installed <n> files, <bytes>` 与 `Transaction completed` 只在 `failed` 为假时打印（前者还要求后端是 pica：native 后端自己会报，Pica 再数一遍就是第二个答案）。`failed` 为真的那条路径是"回滚不完整、写了残留记录"的分支，它只报错误与未恢复的路径，不会先说一句"事务完成"再报失败。
- **报告写不进去**：文件已经落盘、数据库已经记下，报告失败是一条 `InstallReportFailed` 告警，不回滚文件。反过来的顺序不存在——数据库先写，写不进去就是安装失败。
- **失败的升级保留旧记录**：回滚已经把上一版的文件放回来，`db.json` 里那条仍描述这一版；回滚删不掉的新路径用 `db_record_remnant` 合并进同一条记录（`manifest`、`pkgfile`、`installed_at` 不动），`-R` 才有东西可删。首次安装失败没有旧记录，才新建一条，否则那些文件无人认领。
- **报告是尝试记录**：`install-report.json` 写的是最近一次尝试（含 `precheck.optional` 的 `failed` 结果与本次 `dependency_diff`），其中的后端包可能已被同一次失败回滚。它只在写数据库的那两个分支里写：安装提交（`failed=false`）、回滚不完整且留下了待清理记录（`failed=true`）——**一次干净回滚的失败既不写库也不写报告**。判断设备上装了什么看 `db.json`，不看报告，也不看报告缺不缺。
- **Pica 自己的文件不归 payload 管**：`deploy_payload` **在取备份之前**就拒绝要写 Pica 状态目录的载荷（`E_PACKAGE_INVALID`）：`/etc/pica/pica.json`、`/var/lib/pica/{db.json,index.json,install-report.json,db.lck}` 这几个文件，以及 `<lock_file>.d`、`<state_dir>`、`<etc_dir>`（按 `Paths` 的实际取值）和 `<安装根>/usr/lib/pica/scripts/`、`<安装根>/etc/pica/env.d/` 下的任何路径。这些正是本次事务要读写的对象，让载荷落进去等于让包覆盖自己的账本，或者往 `db.lck.d/pid` 里写一个不存在的 pid、让锁被下一个进程判成陈旧；脚本目录里则是 `-R`/`-U` 会执行的 hook。判定既在 walk 之前也在快照之前，所以拒绝时设备上的文件不但没被写，也没有被"复制出去再放回来"——后者会换掉 inode、断开硬链接，而文件内容看起来没变。这是防误撞的护栏，不是权限边界：安装 hook 本来就是 root 任意代码，直接写这些文件拦不住。
- **判定看落点，不看拼写**：上级组件里已有的符号链接、载荷自己要创建的符号链接，都按内核的解析顺序算到最终位置再比较，所以 `usr/lib/demo -> ../../var/lib/pica` 时 `usr/lib/demo/db.json` 会被认成数据库，先放一个 `opt/alias -> ../var/lib/pica` 也会；悬空链接同样算，`create_dir_all` 会顺着它把目录建在它指向的地方。解析不出来的路径（读不出的链接、成环超过 32 跳、名字里带 `..`）在部署前拒绝（`E_PACKAGE_INVALID`），不猜。指向安装根**之外**的符号链接是另一件事：那里没有 Pica 的状态可保护，护栏不据此拒绝，交给 walk 按既有语义拒（`payload symlink escapes the target root`）——这时别的条目可能已经落盘，走的是回滚，而不是"一个字节都没动"。整个检查在 walk 开始之前一次算完，与条目在第几个被走到无关。
- **快照只覆盖 walk 会替换的路径**：`snapshot_payload` 对 `root/` 里的目录只递归、不取备份（walk 对已存在的目录只是往里合并，从不替换目录本身），对文件和符号链接才取；因此"目标处是符号链接指向目录"这类情况不会出现无谓的 unlink + 重建。取备份失败（缓存不可写、目标不可读、目标处是 socket/fifo/设备节点）是 `E_IO`，此时设备上一个字节还没动。
- **恢复按插入逆序**：`restore` 倒着走备份条目。同一路径只会有一次备份；父条目是文件时不可能有子条目（普通文件没有子节点，符号链接指向目录时 walk 也不替换链接本身），所以逆序不需要额外排序就是部署的逆操作。备份表里也有 payload 之外的两类条目——`env.d/<pkg>.env` 与保留的 hook——它们不会与 payload 路径互为父子：护栏已经禁止载荷写进那两个目录。
- **恢复可以重跑**：第一次恢复部分失败时，条目不会被清掉（备份目录也保留），修好故障后再跑一次会补齐；已经恢复过的条目会被再写一遍同样的内容，不删除、也不写入失败版本的内容。
- **升级不删除旧版多出来的文件**：升级就是同 `pkgname` 的一次安装，`db.json` 被新清单覆盖，旧版独有、新版不再发布的路径留在盘上但不再被记录（`-R` 也删不到）。这是既有语义，本次未改。
- **只放回 walk 真正碰过的文件**：`install_payload_with` 把 walk 走过的目标边写边记（`deployed`），回滚时文件名条目只有在这个集合里才放回副本——walk 没走到的文件还是原样，用副本覆盖一遍只会换 inode、断硬链接、白读白写一个大文件。符号链接与空目录例外，因为它们被先 unlink 再重建，失败时名字是**缺失**的，副本是唯一来路。Pica 自己写的文件（`env.d/<pkg>.env`、保留的 hook）走的是 `copy` 而不是 rename，失败也可能已经截断，所以永远放回——**不能把"walk 没报告过"推广到所有文件类型**。
- **三个不同的问题**：①"备份过"只说明快照阶段把目标复制进了 `cache/backup/<pkg>-XXXX/`，不说明目标被改过——walk 可能根本没走到它；②"walk 报告过"说明它被当作已替换，但不等于"必须放回"：Pica 自己 `copy` 出来的 env 与保留 hook 无论如何都要放回，只有 walk 用 rename 换掉的普通文件才按 `deployed` 集合过滤；③"恢复了"只是把本次动过的路径还原，**不构成整次安装的原子性**——多文件之间没有共同提交点，进程中途被杀就是部分旧、部分新。另外，放回一定写新 inode，属主、ACL、xattr、硬链接拓扑都不保证保留。
- **回滚失败要复述**：`rollback` 把放不回去的路径以 `content:<路径> (<原因>)` 收进 `leftovers`，同时保留备份目录（那是唯一副本）；有 leftovers 时错误消息改成"回滚未完成"，不会把部分恢复说成成功。

### 生命周期脚本的时序边界

- **时机**：`script_install` / `script_update` 在 `root/` 部署**之前**执行（步骤 6 早于步骤 7）；`script_remove` 在卸载时执行，回滚时也拿它当补偿。
- **服务型软件**（Komari agent 这类）：更新脚本在部署前停服务，若随后部署失败，回滚只会调用包的 `remove` 脚本补偿，**不会**自动把服务重新拉起来；当前也没有 post-deploy / post-commit 钩子可以等"新二进制就位"再启动。停服务后重启的竞态、半更新状态被启动，都没有协议支持，属于待扩展项。
- **rename 不等于服务更新成功**：同目录 rename 让运行中的旧进程继续持有旧 inode、新进程从新路径启动，这只是文件层面的效果，不代表服务已重启，也不保证在各平台与文件系统上一致。

### 运行时并发约束锁设计

OpenWrt 的包管理容忍度低（尤其是修改同一个清单或写入），故 Pica 要求在执行前持有**全局运行锁**：

1. **原子建立检测**：使用新建具有时序的目录锁 `db.lck.d`。
2. **三种答案，不是两种**：`process_liveness` 返回 `Running` / `Gone` / `Unknown`。`/proc` 在、`/proc/<pid>` 在就是活；`/proc` 在而 `NotFound` 才是死；`/proc` 不存在、被 `hidepid` 挡住、读不了，都是 `Unknown`——**不能读成死**。只有 `Gone` 才清锁（`E_RUNTIME` 只用在清不掉时），`Running` 报 `E_LOCK_BUSY`，`Unknown` 也报 `E_LOCK_BUSY`（`LockUnverified`：说清是"没法确认"，请人工确认后删目录）。拿不准时删锁是唯一不可撤销的动作：代价是另一个正在进行的安装悄悄丢掉自己的写入，而留一把没人持有的锁只花操作员一次 `rm`。
3. **目录里还没有 pid**：`create_dir` 到写 `pid` 之间只有几个系统调用，所以这种目录按年龄判——超过 `ACQUIRE_GRACE`（5 秒）才当遗留物清掉，刚建出来的当"正在被取"，不碰。年龄取自锁目录自身的 mtime；mtime 读不出来（权限、坏时间戳）算 `Unknown`，同样只报忙。
4. **`opkg.lock` 同步规避**：同样只在宿主 pid **确认已死**时才清掉并重试一次；宿主还活着或无法确认（`Unknown`）都报 `E_LOCK_BUSY`。
5. **PID 重用只会更保守**：遗留锁里的 pid 若已被无关进程复用，检查结果是 `Running`，锁被当成在用——要人工确认后删除，不会误删别人的锁。
6. **这就是平台限制**：没有 `/proc`、`/proc` 不可读或被 `hidepid` 遮住的宿主（开发机、裁剪过的容器）上，遗留锁不会自动消失，也不会被误删，需要人工删除 `db.lck.d`；`opkg.lock` 同理。OpenWrt 有 `/proc`，路由上的行为不变。

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
