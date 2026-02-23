# pica-rs 迁移架构说明（v0.1.0）

本文档面向“从 Bash 到 Rust 的保守迁移”，目标是：

- 行为可对照（避免闹笑话）
- 低依赖（适配 OpenWrt 小存储）
- 易维护、易读、可长期演进

---

## 1. 为什么 pica 要这样设计

pica 不是重新发明一套包管理器，而是做 OpenWrt 生态的“上层协调器”：

- `opkg` 仍是实际安装执行者
- pica 负责：元数据约束、兼容判断、来源策略、安装顺序、状态记录

这样做的好处：

- 与 OpenWrt 生态兼容
- 复杂依赖解析交给 opkg
- pica 专注于应用层抽象（selector、pkgrel、repo schema）

---

## 2. Rust 版本设计原则（强约束）

1. **先等价再增强**
   - 第一阶段先保证关键行为可对照。
2. **低依赖优先**
   - 能用 std 就不用外部库。
   - 下载/安装继续调用系统命令（uclient-fetch/wget/curl/opkg/tar）。
3. **可控错误**
   - 所有错误映射为稳定错误码（便于前端/后端）。
4. **强边界**
   - core/cli/pack 分层，避免脚本式全局耦合。

---

## 3. 当前仓库结构

```text
pica-rs/
  Cargo.toml
  crates/
    pica-core/
      src/
        error.rs
        io.rs
        manifest.rs
        repo.rs
        selector.rs
        version.rs
    pica-cli-rs/
      src/main.rs
    pica-pack-rs/
      src/main.rs
```

说明：

- `pica-core`：共享核心（selector/version/manifest/repo/io/error）
- `pica-cli-rs`：设备端 CLI
- `pica-pack-rs`：构建端打包器

---

## 4. Bash 语义在 Rust 中不建议硬搬（并已替代）

1. **jq 管道 + 动态拼接**
   - 替代：`serde_json::Value` + 结构化读写。
2. **trap + 全局状态清理**
   - 替代：`LockGuard` RAII 自动释放锁目录。
3. **函数内定义函数（脚本风格）**
   - 替代：模块化私有函数，按职责拆分。
4. **隐式容错字符串解析**
   - 替代：显式 Selector/Version/Manifest 解析函数。

---

## 5. 已实现范围

### 5.1 `pica-pack-rs`

- `build` 子命令完整迁移。
- 支持平台/架构矩阵产物拆分。
- 构建时重写 manifest 字段并注入 `builddate/size`。
- 文件名规则与 Bash 一致（含 `platform=all` 特例）。
- 继续依赖系统 `tar`（避免引入体积较大的压缩库链）。

### 5.2 `pica-rs`

- 命令：`-S/-Su/-Syu/-Si/-So/-Sp/-U/-R/-Q/-Qi/-Ql`
- repo 同步：严格 schema/filename 校验 + 本地 index/cache 更新
- 包安装：
  - selector 解析
  - repo 候选筛选与版本选择
  - `.pkg.tar.gz` 解包安装
  - 兼容性校验（pica 最低版本、uname、arch、luci）
  - 依赖预检与来源策略（feed/ipk）
  - 生命周期钩子
  - cmd/env 持久化
- 包移除：cmd/opkg/env 清理 + db 更新
- 升级：仅 `source == pica` 项滚动升级
- 报告：`install-report.json` 记录 precheck 与依赖差异
- JSON 输出与锁机制

---

## 6. 仍建议后续增强（非阻断）

1. **回滚策略增强**
   - 当前与 Bash 一致，失败后回滚能力有限。
2. **hook 执行约束**
   - 当前保持兼容直接执行；可选增加白名单/隔离。
3. **下载完整性校验**
   - 可增加 `sha256` 校验字段与强校验。
4. **更细粒度单元测试**
   - 尤其是安装事务的失败分支与兼容门禁分支。

---

## 7. 结论

当前 Rust 版本已经具备替换 Bash 主流程的功能基础。

建议上线方式：

- 先灰度用于 `-S/-Sp/-U/-R`
- 再放开 `-Si/-Su/-Syu`
- 持续收集 OpenWrt 目标设备上的行为差异并迭代

---

## 8. 解耦推进记录

- 现阶段与第一阶段解耦详情见：`docs/decoupling-phase1.md`
- 该文档用于保留“改造前快照 + 第一阶段落地变更 + 验证结果”，便于后续排错与继续拆分。
