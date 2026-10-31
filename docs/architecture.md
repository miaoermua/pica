# 架构总览

Pica 有两个二进制：构建端 `pica-pack` 把 pkgdir 打成产物和索引，设备端 `pica` 同步索引并安装产物。

| crate           | 产出        | 职责                                  |
| --------------- | ----------- | ------------------------------------- |
| `pica-pkg-core` | 库          | 协议类型、命名规则、协议常量、基础 IO |
| `pica-pkg-pack` | `pica-pack` | 构建产物，生成索引                    |
| `pica-pkg-cli`  | `pica`      | 同步、安装、升级、卸载、查询          |

## 数据流

```text
pkgdir -> pica-pack -> <outdir>/index.json + <backend>/{index.json,*.pkg.tar.gz}
                              | 发布
                              v
pica -S -> /var/lib/pica/index.json（同步缓存）
pica -S <app> -> 下载并校验 sha256 -> 安装 -> /var/lib/pica/db.json
```

## 安装步骤

代码在 `crates/pica-pkg-cli/src/commands/install/`，入口是 `pipeline.rs` 的 `pkgfile_into`。

| 步骤 | 内容                                         | 文件                     | 函数                            | 失败时                                   |
| ---- | -------------------------------------------- | ------------------------ | ------------------------------- | ---------------------------------------- |
| 1    | 解包，读取并检查 manifest                    | `pipeline.rs`            | `pkgfile_into`                  | 无需回滚                                 |
| 2    | 校验产物与本机兼容                           | `precheck.rs`            | `validate_package`              | 无需回滚                                 |
| 3    | 依赖预检                                     | `deps.rs`                | `precheck_dependencies`         | 无需回滚                                 |
| —    | 展示计划；`--dry-run` 在此结束，否则询问确认 | `pipeline.rs`            | `install_unpacked`              | 无需回滚                                 |
| 4    | 记录后端已装列表                             | `pipeline.rs`            | `run_install`                   | 无需回滚                                 |
| 5    | 安装依赖容器                                 | `deps.rs`                | `install_dependency_containers` | 移除本次新增的包                         |
| 6    | 运行生命周期脚本，替换保留脚本               | `hooks.rs`               | `execute_scripts`               | 运行包的 `remove` 脚本，放回旧的保留脚本 |
| 7    | 部署 `root/` 与 `.env`                       | `payload.rs`、`guard.rs` | `deploy_payload`                | 恢复被替换的文件，删除新建的路径         |
| 8    | 写 `db.json` 与安装报告                      | `transaction.rs`         | `record_installation`           | 同步骤 7                                 |
| —    | 提交后删除旧版独有的文件                     | `transaction.rs`         | `remove_orphans`                | 只告警，不回滚                           |

被替换文件的备份在 `backup.rs`（`PayloadBackup`），回滚在 `transaction.rs`（`InstallTransaction::rollback`），细节见 [developer.md](./developer.md)。测试按同样的步骤分在 `pipeline_tests/` 下。

## 从哪里开始改

| 想改的内容         | 位置                                        |
| ------------------ | ------------------------------------------- |
| 命令与参数         | `crates/pica-pkg-cli/src/main.rs`、`app.rs` |
| 用户可见文案       | `crates/pica-pkg-cli/src/i18n.rs`           |
| 选包规则           | `crates/pica-pkg-cli/src/candidate.rs`      |
| 下载地址与抓取策略 | `crates/pica-pkg-cli/src/source.rs`         |
| opkg / apk 调用    | `crates/pica-pkg-cli/src/pkgmgr/`           |
| 协议路径与常量     | `crates/pica-pkg-core/src/layout.rs`        |
| 名字规则           | `crates/pica-pkg-core/src/naming.rs`        |
| 索引校验           | `crates/pica-pkg-core/src/repo.rs`          |
