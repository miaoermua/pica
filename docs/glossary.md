# 术语表

| 术语      | 含义                                                                    |
| --------- | ----------------------------------------------------------------------- |
| 产物      | `pica-pack` 生成的 `.pkg.tar.gz`                                        |
| 容器      | 产物 `pkgs/` 里的 `.ipk` / `.apk`，交给系统包管理器安装                 |
| 后端      | 拥有包内文件的一方，即 `pkgmgr`：`opkg`、`apk` 或 `pica`                |
| 渠道      | `db.json` 的 `source`，表示包怎么拿到的：`pica`、`local`、`url`、`feed` |
| feed      | 系统包管理器自己配置的软件源，不是 Pica 软件源                          |
| `appname` | 命令行选择包用的名字                                                    |
| `pkgname` | 包在源内的唯一身份，可带 `-bin`、`@<谁>`                                |

## 同一个词的不同含义

**`pica`**：产品名；后端名（`pkgmgr = pica`）；渠道值（`source = pica`）；文件名后缀 `-pica`；manifest 里的 `pica` 字段，表示要求的最低 CLI 版本。

**`index.json`**：源根的入口描述；`<backend>/index.json` 后端索引；设备上 `/var/lib/pica/index.json` 同步缓存。三者结构不同。

**`@`**：manifest 键里的后端限定（`base@apk`）；`pkgname` 里的作者标识（`demo@alice`）；选择器里的分支分隔符（`demo@stable`）。

**`url`**：`repos[].url` 是源根地址；manifest 和索引条目的 `url` 是项目主页；`download_url` 才是产物下载地址。
