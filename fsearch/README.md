# fsearch — 极速目录 / 文件名关键词检索

在指定目录（或当前目录）下递归检索**目录名与文件名**，命中即输出。为「在 D 盘这种大目录里秒级捞文件」而写：多线程并行遍历、边遍历边匹配边输出、匹配走零分配字节子串查找。

```powershell
fsearch 报告                    # 当前目录下所有名字含“报告”的文件和目录
fsearch -d D:\ 设计 -t psd,blend # D 盘里所有 psd / blend 文件
fsearch -d D:\ 截图 -n 20        # 只列前 20 条，够了就立刻停
```

---

## 一、特性

- **快**：`ignore` 库的并行遍历器，线程数默认取 CPU 核数；匹配在 OS 原生字节上做，不为每个文件名分配 `String`。
- **边遍历边输出**：不先收集全量再过滤，内存占用恒定；`-n` 达到上限立即终止整棵遍历树，而不是事后截断。
- **匹配分层**：纯 ASCII 关键词走 `memchr2` 预筛 + `eq_ignore_ascii_case`；纯中文等无大小写字符的关键词退化为精确子串（语义等价、更省）；只有含非 ASCII 大小写字母时才做 Unicode 大小写折叠。
- **路径兼容**：`src/main`、`src\main` 都能匹配；关键词里带 `/` 或 `\` 时自动切换为相对路径匹配。
- 支持 `.gitignore` / `.ignore` 规则（`-I` 可关闭）、隐藏项（`-H`）、glob 排除（`-x`，可重复）、扩展名过滤、遍历深度、正则。
- 输出成体系：普通文本、`--long`（带大小）、`--json`（JSON Lines，便于管道）、`--absolute`、`--sort`（按路径排序）。
- 统计信息走 **stderr**，不污染 `stdout` 管道：`匹配 84 项（文件 40 · 目录 44）｜遍历 357,842 项｜3.09s｜16 线程｜跳过 47 个无权限项`。

## 二、构建与安装

```powershell
cd D:\AI\rust学习\fsearch
cargo build --release --offline        # 产物约 2.1 MB（lto + strip）
```

把它加入 PATH，之后任意目录直接 `fsearch`：

```powershell
$dir = 'D:\AI\rust学习\fsearch\target\release'
[Environment]::SetEnvironmentVariable(
  'Path',
  [Environment]::GetEnvironmentVariable('Path','User') + ";$dir",
  'User')
```

或者用 cargo 装到 `%USERPROFILE%\.cargo\bin`（该目录通常已在 PATH 里）：

```powershell
cargo install --path . --offline
```

## 三、参数

```
Usage: fsearch [OPTIONS] <KEYWORD>

  -d, --dir <DIR>        起始目录（默认当前目录）
  -s, --case-sensitive   区分大小写
  -r, --regex            关键词按正则解释
  -p, --full-path        匹配相对路径（而不是仅文件名）
  -t, --type <EXT>       只匹配这些扩展名，逗号分隔，可重复（-t rs,toml）
  -f, --files-only       只列文件
  -D, --dirs-only        只列目录
  -H, --hidden           包含隐藏项（以 . 开头）
  -I, --no-ignore        不遵守 .gitignore / .ignore
  -x, --exclude <GLOB>   排除路径 glob，可重复（-x node_modules -x target）
  -j, --threads <N>      遍历线程数（默认自动）
      --max-depth <N>    最大深度（1 = 只看直接子项）
  -n, --max-results <N>  最多输出多少条（0 = 不限制）
      --sort             排序后输出（默认按发现顺序，最快）
      --absolute         输出绝对路径
      --json             JSON Lines 输出
      --long             显示文件大小
  -q, --quiet            不打印统计
```

## 四、实测（Windows 11，16 线程，release）

| 检索根 | 遍历条目 | 命中 | 耗时 | 吞吐 |
| --- | ---: | ---: | ---: | ---: |
| `C:\Windows` | 357,842 | 84 | 2.6–3.1 s | ~12–14 万项/秒 |
| `C:\` | 456,751 | 769 | 3.13 s | ~14.6 万项/秒 |
| `D:\` | 2,735,454 | 358 | 2.88 s | ~95.0 万项/秒（页面缓存热） |

同一根目录、同一关键词的 PowerShell 等价写法：

```powershell
Get-ChildItem C:\Windows -Recurse -Force -ErrorAction SilentlyContinue |
  Where-Object { $_.Name -like '*node*' }      # 70.29 s，命中 85
```

`fsearch node -d C:\Windows` 只用约 **3 s**（命中 84，差的 1 条落在 47 个无权限/读取失败项里）。约 **23×**。

> 短语境：`-n 20` 这类「找到够用就停」的场景更快。上例 `fsearch System32 -d C:\Windows -n 3` 扫到 15.9 万项、0.69 s 就收工。

## 五、实现要点

1. **并行遍历**：`ignore::WalkBuilder::build_parallel`，每个工作线程一个 visitor，各持有 32 KB 输出缓冲（`Batch`，`Drop` 时 flush，避免丢尾部）。
2. **并发输出**：`stdout` 用 `Mutex<BufWriter<io::Stdout>>` 包住（`StdoutLock` 不是 `Send`，不能跨线程持有）。
3. **精确的 `-n`**：`AtomicUsize::fetch_update`/`compare_exchange_weak` 抢名额，抢满后置 `stop` 标志，visitor 返回 `WalkState::Quit` 立即结束遍历。
4. **匹配器**（`src/matcher.rs`）：按关键词形态在构建期选定 `Exact / AsciiCi / UnicodeCi` 之一；路径匹配先按原生分隔符比一次，未命中且含 `\` 时把 `\` 归一成 `/` 再比一次。
5. **`--sort` 才收集**：只有排序模式把结果收进 `Vec`（上限 200 万条），默认模式完全不缓存结果。

## 六、已知限制

- 只按**名称 / 相对路径**匹配，不读文件内容（那是 `rg` 的活）。
- 不跟随符号链接（避免目录环）。
- `--sort` 需要全量收集，超大目录下内存会涨；结果超过 200 万条会提示截断。
- 退出码：`0` 正常结束（含 0 命中），`2` 参数错误；遍历中的无权限/读取失败计为「跳过项」，不影响退出码。

## 七、测试

```powershell
cargo test --offline          # 7 个匹配器单元测试
```

夹具目录 `D:\AI\rust学习\fsearch-demo`：中文名、大小写、正则、扩展名、隐藏项、`node_modules` 排除、路径匹配均覆盖。
