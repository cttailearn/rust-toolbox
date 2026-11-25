# AGENTS.md

本文件是给 AI 编码代理的工作约定。在本仓库内做任何代码改动前先读本文件。

## 仓库概览

本仓库是 Rust 小工具集：**一个目录一个工具**，每个工具独立成 crate，由根目录的 Cargo workspace 统一构建。

- `fsearch/` — 并行目录/文件名关键词检索 CLI（`anyhow` + `clap` + `ignore` + `memchr` + `regex`）
- 新增工具：在仓库根目录 `cargo new <tool>`，把名字加进根 `Cargo.toml` 的 `members`，并在根 `README.md` 的工具表里加一行
- 所有 crate 使用 `edition = "2024"`；`[profile.release]` 统一放在根 `Cargo.toml`（成员 crate 内的配置会被忽略）

> 开发者本地另有练习 crate `hello_rust/`，不属于本仓库。

## 构建与验证

以下命令**全部在仓库根目录执行**，提交前必须全部通过：

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all
```

根 `Cargo.toml` 的 `[profile.release]` 已开启 `lto = "thin"`、`codegen-units = 1`、`panic = "abort"`，改动后可用 `cargo build --release` 验证。

## Rust 设计准则（强制）

以下五条是本项目的核心约束。新增代码必须遵守；修改既有代码时若与之冲突，先说明再改。

### 1. 优先使用 Newtype

用单字段 `struct` 给同构的原始类型赋予语义，禁止裸 `String`/`usize`/`PathBuf` 在接口中传递多种含义。

- 校验收敛到构造函数：实现 `TryFrom`、`FromStr`、`new`，保证「构造成功 = 一定合法」，此后的代码无需重复校验。
- 字段默认私有，只暴露必要的方法；不要让调用方绕过构造直接拼装。
- newtype 上实现 `Deref`/`AsRef` 仅在确实方便且不破坏不变式时才用。

```rust
// 反例：两个 &str 参数，顺序传反也能编译
fn search(root: &str, pattern: &str) {}

// 正例：类型即文档，顺序错误无法通过编译
struct RootPath(PathBuf);
struct Pattern(String);

impl TryFrom<&str> for Pattern {
    type Error = anyhow::Error;
    fn try_from(s: &str) -> Result<Self> {
        if s.is_empty() { anyhow::bail!("pattern 不能为空") }
        Ok(Self(s.to_owned()))
    }
}

fn search(root: &RootPath, pattern: &Pattern) {}
```

### 2. 避免不必要的 `Arc`

`Arc` 只用于「确实需要跨线程共享同一份数据」的场景。按以下顺序选择方案，只有前几条都不成立时才引入 `Arc`：

1. 直接传值或传借用（`&T`、`&mut T`）；
2. 拆分数据所有权，让每个线程持有自己那一片（`split_at_mut`、`chunks_mut`、`drain`、`into_iter` 按范围切分）；
3. 小数据（配置、标志位）直接 `Clone` 给每个线程，复制成本远低于同步成本；
4. 结果通过 channel 回传（见第 3 条），不共享可变容器；
5. 只读大块数据确实要被多个线程同时引用时才用 `Arc<T>`。

禁止把 `Arc<Mutex<T>>` 当作默认的跨线程通信方式。需要可变共享状态时，先自问：能否改成「每个线程算自己的部分，最后汇总」？能就不要锁。

### 3. 线程之间共享数据时优先使用 Channel

- 并行任务的结果通过 channel 回传：`std::sync::mpsc`（单消费者）或 `crossbeam-channel`（多生产者/多消费者/select）。
- 不要在多个线程里 `lock()` 同一个 `Vec` 来收集结果；让 worker 只负责生产并 `send`。
- 消费者独占接收端，负责去重、排序、打印等串行逻辑。
- 高频小消息要批量发送，降低同步与唤醒开销；边界清晰的任务优先「一个线程一次发送一批」。
- channel 关闭（`Sender` 全部 drop）是天然的结束信号，利用它而不是额外的原子标志位。

```rust
// 反例：共享可变容器
let results = Arc::new(Mutex::new(Vec::new()));
// 正例：worker 生产，主线程消费
let (tx, rx) = std::sync::mpsc::channel();
std::thread::spawn(move || {
    for item in chunk { tx.send(item).unwrap(); }
});
let all: Vec<_> = rx.into_iter().collect();
```

### 4. 用 Trait 和类型系统表达抽象

- 行为抽象用 trait；数据分支用 `enum`。不要用「一个带 `kind: u8` 字段的 struct」表达状态机。
- 参数优先泛型 / `impl Trait`，避免 `&dyn Trait` 的动态分发与间接调用，除非确实需要异构集合（如 `Vec<Box<dyn Trait>>`）。
- 用 trait bound 描述能力需求（`R: Read + Seek`），把「运行时会失败」的检查前移到「编译期不通过」。
- 非法状态应不可表示：用 typestate 或各状态独立的类型，而不是「所有字段都 Option + 运行时 assert」。
- 对外 API 用 sealed trait 限制实现范围，防止下游破坏不变式。

### 5. 高频循环中避免不必要的堆分配

- 循环外分配、循环内复用：`let mut buf = Vec::with_capacity(cap);` → 每轮 `buf.clear()`。
- 能用 `&str` / `&[u8]` 切片就不要构造 `String` / `Vec<u8>`；仅当需要跨作用域持有时用 `Cow` 或 `to_owned()`。
- 禁止在热循环里 `format!`、`to_string()`、`to_lowercase()`、`collect::<Vec<_>>()` 只是为了下一行再遍历。
- 迭代器链直接消费（`for`/`fold`/`try_for_each`），避免中间 `collect` 产生临时 `Vec`。
- 需要临时缓冲区时用栈数组、`SmallVec` 或 `thread_local!` 复用，而非每次 `Vec::new()`。
- 需要拼接路径时优先 `PathBuf` 复用（`push`/`set_file_name`），避免 `format!("{}/{}", a, b)`。
- 只优化经过测量的热点：先用 `bench`/`perf`/实际数据确认瓶颈，再做上述改造；不做无依据的「优化」。

## 编码风格

- 错误处理：应用层用 `anyhow::Result` + `context`，库层用 `thiserror`；不要在可恢复路径上 `unwrap`/`expect`（构造常量、测试除外）。
- 注释解释「为什么」，不复述「做了什么」；公共项写文档注释。
- 优先小函数 + 明确类型；避免超长函数里混用多种所有权策略。
- 不引入新依赖除非确有必要，引入前说明理由与替代方案。
