# rust-toolbox

Rust 小工具集：**一个目录一个工具**，每个工具独立成 crate，由根目录的 Cargo workspace 统一构建。

## 工具列表

| 目录 | 工具 | 说明 |
| --- | --- | --- |
| [`fsearch/`](fsearch/README.md) | fsearch | 并行目录 / 文件名关键词检索 CLI。`C:\Windows` 35.8 万项 2.6–3.1 s，`D:\` 273.5 万项 2.9 s |
| — | — | 下一个工具：`cargo new <tool>`，加入 `members` 即可 |

## 快速开始

```bash
cargo build --release          # 所有工具，产物在 target/release/
cargo test --workspace
```

只构建/运行某一个工具：

```bash
cargo run --release -p fsearch -- 报告 -d D:\ -n 20
```

## 新增一个工具

```bash
cargo new --bin mytool         # 在仓库根目录创建
# 1. 把 "mytool" 加进根 Cargo.toml 的 members
# 2. 在根 README.md 的「工具列表」加一行
cargo build --release          # 统一构建，release 配置已在根 Cargo.toml 里
```

约定见 [AGENTS.md](AGENTS.md)：newtype 优先、线程间用 channel 而非 `Arc<Mutex<_>>`、高频循环里避免堆分配。

## 目录结构

```
Cargo.toml        # workspace 根：成员列表 + 全仓库 release 配置
AGENTS.md         # AI 编码代理的工作约定
fsearch/          # 工具一：并行关键词检索
  Cargo.toml
  src/main.rs     # CLI、并行遍历、输出
  src/matcher.rs  # 零分配匹配器 + 单元测试
  README.md       # 参数表、实测数据、实现要点
```

> `AGENTS.md` 里提到的本地练习 crate `hello_rust/` 不在本仓库中。
