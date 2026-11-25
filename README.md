# fsearch

**极速目录 / 文件名关键词检索 CLI**（Rust，并行遍历）。

在指定目录下递归检索**目录名与文件名**，命中即输出。为「在 D 盘这种千万级条目的大目录里秒级捞文件」而写：多线程并行遍历、边遍历边匹配边输出、匹配直接跑在 OS 原生字节上。

```powershell
fsearch 报告                      # 当前目录下所有名字含“报告”的文件和目录
fsearch -d D:\ 报告 -n 20          # 在 D 盘检索，够了就停
fsearch -d D:\ 设计 -t psd,blend   # 只找 psd / blend 文件
fsearch -r '^20\d\d' -d D:\AI      # 正则匹配
fsearch src/main -d .              # 带分隔符的关键词自动按相对路径匹配
```

## 实测

| 检索根 | 遍历条目 | 命中 | 耗时 | 吞吐 |
| --- | ---: | ---: | ---: | ---: |
| `C:\Windows` | 357,842 | 84 | 2.6–3.1 s | ~12–14 万项/秒 |
| `C:\` | 456,751 | 769 | 3.13 s | ~14.6 万项/秒 |
| `D:\` | 2,735,454 | 358 | 2.88 s | ~95 万项/秒 |

同样的 `Get-ChildItem -Recurse | Where-Object Name -like '*node*'` 扫 `C:\Windows` 需要 **70.29 s**，约 23 倍差距。

## 构建

```bash
cargo build --release        # 产物 fsearch/target/release/fsearch(.exe)，约 2.1 MB
cargo test                   # 7 个匹配器单元测试
cargo clippy --all-targets -- -D warnings
```

## 安装

```bash
cargo install --path fsearch
# 或把 fsearch/target/release 加入 PATH
```

## 仓库结构

```
AGENTS.md           # 本仓库 AI 编码代理的工作约定
fsearch/            # 唯一的 crate：参数解析、并行遍历、匹配器、输出
  src/main.rs
  src/matcher.rs
  README.md         # 完整参数表、实现要点、限制
```

> `AGENTS.md` 中提到本地练习 crate `hello_rust/` 属于开发者的本地工作区，不包含在本仓库中。

完整参数与实现说明见 [fsearch/README.md](fsearch/README.md)。
