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

### 0. 初始化 (`setup`)

第一次使用、或者想换软件源与界面语言时：

```bash
pica setup
```

向导按顺序问三件事：

1. 界面语言：方向键 ↑/↓ 移动、Enter 确认，直接回车就是 English；也可以用 `pica setup --lang zh-CN` 跳过这一问。选定后剩下的问题和输出立刻换成该语言，并存进 `pica.json` 的 `language`。
2. 软件源：官方源（构建里配了才出现）、自定义源根地址，或者原样保留已配置的源。直接回车选默认项（官方源在时选官方源，否则选"保留已配置的源"，没有配置时就是输入地址）。已有源会先列出来，不会被静默覆盖；地址读不到或没有可用后端时会说明原因并重新询问。
3. 随后自动完成：抓该源的 `index.json` 与其中的后端索引，按"本机支持的后端 × 源声明的后端"求交集写进 `repos[].backends`，再同步。

后端不需要用户手填：交集为空时向导报错并放弃该地址（不会退回成"源声明的全部后端"，也不会写入 `auto` 这种折中值）。

**同步失败时什么都不写**：配置文件、`state/index.json` 和各源的 `cache/repos/*.json` 在最后一起提交，失败则全量回滚，不会留下"看起来成功、其实没索引"的半成品。

非交互环境（脚本、容器、没有终端的远程会话）不会等待输入，而是直接报错并指出改用配置文件：

```bash
# 等价于向导写出的结果
cat > /etc/pica/pica.json <<'EOF'
{
  "language": "zh-CN",
  "repos": [
    { "name": "main", "url": "https://example.com/pica/r", "backends": ["opkg", "pica"] }
  ]
}
EOF
pica -S
```

只想临时换个语言看一眼，不动配置：

```bash
pica --lang en-US -Si hello
```

`--help`、`--version`、`-Qi` 的字段名以及 `--json-errors` 的 `code` 不随语言变化，脚本可以照旧解析。

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

软件源在主配置里是**根地址**，不是索引文件本身：

```json
{
  "repos": [
    {
      "name": "main",
      "url": "https://example.com/pica/r",
      "backends": ["opkg", "pica"]
    }
  ]
}
```

- `url` 指向源的根目录，客户端自己去找 `index.json`（入口描述）和它指出的后端索引，不假设固定的域名或路径。
- `backends` 是这个源启用的后端组合，只抓这些后端的索引。`opkg + pica`、`apk + pica` 都合法，按你设备要用的后端写。源没有声明你写的某个后端时，`-S` 会直接报错并列出它实际声明了哪些，不会拿别的后端顶替。
- 想换组合，改完 `backends` 后重新 `pica -S`：只有被启用的后端的索引在本地。
- 旧配置（只有 `name` / `url`，源目录下放 `repo.json`）继续可用，行为与以前一致。若源只提供新的 `index.json` 而你又没写 `backends`，客户端会退回入口描述并抓取它声明的所有后端。

局域网里的自建源可以直接用明文 HTTP，例如 `http://192.168.1.10/pica/r`：本机回环、私有网段与链路本地地址默认允许。**公网**地址默认只接受 `https`，确需明文时在该源条目上加 `"allow_insecure_http": true`；这是每个源自己的选择，不是全局开关。

### 2. 通过软件源安装 (`-S <pkg>`)

安装仓库内已知的软件：

```bash
pica -S miaoer-app
```

输出示例：
```text
:: Downloading miaoer-app (0.2.8-1) from main...
:: Installing miaoer-app (0.2.8-1)
::   Package installed size: 4.2 MiB
::   Download size: 1.1 MiB
:: Checking declared dependencies...
::   luci-app-miaoer-app (feed)
:: Installed 128 files, 4.2 MiB
:: Transaction completed
```

- 第一行说清这次事务对这个包做了什么：首装是 `Installing`，还装过则是 `Upgrading miaoer-app (0.2.7-1 -> 0.2.8-1)`（降级同理，写 `Downgrading`），版本相同则是 `miaoer-app 0.2.8-1 is already installed; reinstalling`。
- 缩进的两行大小来自仓库索引，是**包自称**的数字；索引没给或给了 0（表示没测量）就不打印。本地文件与 URL 安装没有索引，因此没有这两行。
- `Checking declared dependencies...` 下面逐条列出这次真正要动手的依赖及其来源：`(feed)` 表示要从软件源装，`(packaged)` 表示用包内 `pkgs/` 里的容器。已装的依赖不会列出，被跳过的可选依赖也不会列出。
- 最后一行 `Installed ...` 是实际写入量：文件数按载荷里的条目算，字节数按普通文件算，目录不计。用系统后端（`opkg`/`apk`）安装时没有这一行——文件由后端铺放，Pica 不重复统计，也不虚报一份自己的结果。
- 同一个包在索引里按架构各有一条时，只有本机能跑的那条会被选中：`arch` 与设备完全不兼容的条目直接不参与挑选，因此不会出现下载完才在 precheck 报 `unsupported arch`。`arch = all` 的条目任何时候都参与；设备架构探测不出来时也不排除任何条目，仍由 precheck 提示。

> 想要确认应用信息再安装？可以通过 `pica -Si miaoer-app` 快速检视。

### 3. 本地直装 / URL 在线安装 (`-U`)

如果有人发送了你一个构建好的 pica 包，或是你在网页版应用商店复制了安装包的直接下载链接，均能通过 `-U` (Update/Upload) 完成安装：

```bash
# 从本地路径安装
pica -U /tmp/miaoer-app-0.2.8-1-all-opkg.pkg.tar.gz

# 从网络直链安装
pica -U https://example.com/downloads/miaoer-app-0.2.8-1-all-opkg.pkg.tar.gz
```

### 4. 一键卸载应用 (`-R`)

按“应用包”的粒度将关联资源和子插件一并移除，再也不用逐条运行 `opkg remove`：

```bash
pica -R miaoer-app
```

输出示例：
```text
:: Removing miaoer-app (0.2.8-1)
:: Removing opkg package: miaoer-app
:: Removing opkg package: luci-app-miaoer-app
:: Transaction completed
```

- 头部里的版本是安装时记进数据库的那个版本，不是源里现在提供的版本；记录里没有版本时只写包名。
- 若这个包是用 `root/` 载荷铺放的，`Transaction completed` 之前会多一行实际结果，例如 `Removed 12 files`；被系统后端仍认领的文件不会删除，改成 `Removed 10 files; kept 2 files owned by opkg`。已经不在的文件既不算删除也不算保留。

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
- `pica`：Pica 自己铺放 `root/` 里的文件、执行生命周期脚本，不做依赖求解。这类包不带 `pkgs/`——pica 不解析容器格式，带了的归档会被直接拒绝。

> 安装若中途失败，Pica 会回滚已装的依赖与已铺放的文件；若回滚也无法完成，会列出残留路径供手动清理。

### manifest 里的 `@` 与命令行里的 `@` 不是一回事

manifest 里的 `<key>@<backend>` 是**后端限定**（`backend` 取 `opkg` / `apk` / `pica`），由 `pica-pack` 打包时消费，例如 `base@apk = <该依赖在 apk 源里的名字>`。它表示“这个后端用这一组值”，与命令行选后端无关。

命令行选择器里的 `@` 是另一个含义：`appname@branch`。所以 `pica -U demo@apk` 是“appname = demo、branch = apk”，**不是**“用 apk 后端安装”。要选后端请显式写 `--pkgmgr apk`：

```bash
pica --pkgmgr apk -U demo        # 用 apk 后端装 demo
pica -U demo@apk                 # 装 demo 的 apk 分支
```

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
