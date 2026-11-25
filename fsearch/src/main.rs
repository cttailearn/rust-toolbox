//! fsearch —— 极速目录/文件名关键词检索 CLI
//!
//! 设计思路：
//! 1. **并行遍历**：用 `ignore` 的并行 walker（内部基于 crossbeam），按目录粒度
//!    多线程下钻，Windows 上尤其受益于并发的目录读取；
//! 2. **边遍历边匹配边输出**：不先收集全量条目再过滤，命中即写缓冲，内存占用恒定；
//! 3. **零分配匹配**：文件名直接取 OS 字节做 SIMD 子串查找（见 matcher.rs），
//!    常见的 ASCII / 中文关键词都不产生临时 String；
//! 4. **批量写出**：每个线程 32KB 缓冲，凑满才抢一次 stdout 锁，避免逐行加锁；
//! 5. **默认遵守 .gitignore / 隐藏规则**，少遍历无关目录（`-I` 可关闭）；
//! 6. **可提前终止**：`-n` 达到上限后立刻 Quit，不再空扫剩余目录。

use std::fmt::Write as _;
use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Context;
use clap::Parser;
use ignore::overrides::OverrideBuilder;
use ignore::{WalkBuilder, WalkState};

mod matcher;
use matcher::Matcher;

/// 排序模式下最多收集的条目数，防止极端情况吃光内存
const COLLECT_CAP: usize = 2_000_000;
/// 每个线程的写缓冲阈值
const FLUSH_AT: usize = 32 * 1024;

#[derive(Debug, Parser)]
#[command(
    name = "fsearch",
    version,
    about = "极速目录/文件名关键词检索：并行遍历，边遍历边匹配边输出",
    after_help = "示例:\n  \
fsearch 报告                        当前目录递归查找名称含“报告”的文件和目录\n  \
fsearch -d D:\\ 截图 -n 20           在 D 盘查找，最多列出 20 条\n  \
fsearch -d D:\\ 设计 -t psd,blend     只看 psd / blend 文件\n  \
fsearch -r '^20[0-9][0-9]-' -d D:\\   正则匹配文件名\n  \
fsearch 项目/src --sort              关键词含路径分隔符时自动匹配相对路径\n  \
fsearch -x node_modules -x target     跳过 node_modules 和 target 目录"
)]
struct Args {
    /// 关键词（默认不区分大小写的子串匹配；含 / 或 \\ 时自动匹配相对路径）
    keyword: String,

    /// 起始目录（默认当前目录）
    #[arg(short = 'd', long = "dir", value_name = "DIR", default_value = ".")]
    dir: PathBuf,

    /// 区分大小写
    #[arg(short = 's', long)]
    case_sensitive: bool,

    /// 关键词按正则表达式解释
    #[arg(short = 'r', long)]
    regex: bool,

    /// 匹配相对路径（而不是仅文件名）
    #[arg(short = 'p', long)]
    full_path: bool,

    /// 只匹配这些扩展名，逗号分隔，可重复（如 -t rs,toml）
    #[arg(short = 't', long = "type", value_name = "EXT", value_delimiter = ',')]
    exts: Vec<String>,

    /// 只列文件
    #[arg(short = 'f', long, conflicts_with = "dirs_only")]
    files_only: bool,

    /// 只列目录
    #[arg(short = 'D', long)]
    dirs_only: bool,

    /// 包含隐藏文件和目录（以 . 开头）
    #[arg(short = 'H', long)]
    hidden: bool,

    /// 不遵守 .gitignore / .ignore 等忽略规则
    #[arg(short = 'I', long)]
    no_ignore: bool,

    /// 排除路径的 glob 模式，可重复（如 -x node_modules）
    #[arg(short = 'x', long, value_name = "GLOB")]
    exclude: Vec<String>,

    /// 遍历线程数（默认自动）
    #[arg(short = 'j', long, value_name = "N")]
    threads: Option<usize>,

    /// 最大遍历深度（1 = 只看直接子项）
    #[arg(long, value_name = "N")]
    max_depth: Option<usize>,

    /// 最多输出多少条结果（0 = 不限制）
    #[arg(short = 'n', long, value_name = "N", default_value_t = 0)]
    max_results: usize,

    /// 排序后输出（默认按并行发现的顺序输出，最快）
    #[arg(long)]
    sort: bool,

    /// 输出绝对路径
    #[arg(long)]
    absolute: bool,

    /// 以 JSON Lines 输出（便于脚本消费）
    #[arg(long)]
    json: bool,

    /// 显示文件大小
    #[arg(long)]
    long: bool,

    /// 不打印统计信息
    #[arg(short = 'q', long)]
    quiet: bool,
}

/// 与输出/过滤相关的配置，遍历线程只读共享。
struct Cfg {
    full_path: bool,
    files_only: bool,
    dirs_only: bool,
    exts: Vec<String>,
    json: bool,
    long: bool,
    absolute: bool,
    max_results: usize,
    sort: bool,
    quiet: bool,
}

/// 所有遍历线程共享的状态。
struct Shared {
    matcher: Matcher,
    root: PathBuf,
    cfg: Cfg,
    scanned: AtomicU64,
    errors: AtomicU64,
    matched_files: AtomicU64,
    matched_dirs: AtomicU64,
    printed: AtomicUsize,
    stop: AtomicBool,
    out: Out,
    collected: Mutex<Vec<Item>>,
}

struct Item {
    path: PathBuf,
    is_dir: bool,
    sort_key: Vec<u8>,
}

impl Shared {
    /// 原子地占用一个输出名额，确保 `-n` 严格生效。
    fn reserve_slot(&self) -> bool {
        if self.cfg.max_results == 0 {
            return true;
        }
        loop {
            let cur = self.printed.load(Ordering::Relaxed);
            if cur >= self.cfg.max_results {
                self.stop.store(true, Ordering::Relaxed);
                return false;
            }
            if self
                .printed
                .compare_exchange_weak(cur, cur + 1, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
        }
    }
}

/// 线程安全的 stdout 写出器。
struct Out {
    inner: Mutex<BufWriter<io::Stdout>>,
    broken: AtomicBool,
}

impl Out {
    fn new() -> Self {
        Self {
            inner: Mutex::new(BufWriter::with_capacity(64 * 1024, io::stdout())),
            broken: AtomicBool::new(false),
        }
    }

    /// 整块写入（不会出现半行交错）。返回 false 表示管道已关闭。
    fn write(&self, data: &[u8]) -> bool {
        if data.is_empty() {
            return true;
        }
        let mut w = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(err) = w.write_all(data)
            && err.kind() == io::ErrorKind::BrokenPipe
        {
            self.broken.store(true, Ordering::Relaxed);
            return false;
        }
        true
    }

    fn flush(&self) {
        let mut w = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let _ = w.flush();
    }
}

/// 每个线程独立的写缓冲，线程结束时（Drop）自动 flush，保证不丢尾部结果。
struct Batch {
    shared: Arc<Shared>,
    buf: Vec<u8>,
}

impl Batch {
    fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            buf: Vec::with_capacity(FLUSH_AT + 1024),
        }
    }

    fn flush(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        if !self.shared.out.write(&self.buf) {
            self.shared.stop.store(true, Ordering::Relaxed);
        }
        self.buf.clear();
    }
}

impl Drop for Batch {
    fn drop(&mut self) {
        self.flush();
    }
}

fn main() {
    let args = Args::parse();
    if let Err(err) = run(args) {
        eprintln!("fsearch: {err:#}");
        std::process::exit(1);
    }
}

fn run(args: Args) -> anyhow::Result<()> {
    if args.keyword.is_empty() {
        anyhow::bail!("关键词不能为空");
    }
    let started = Instant::now();

    let root = std::path::absolute(&args.dir)
        .with_context(|| format!("无法解析目录: {}", args.dir.display()))?;
    if !root.is_dir() {
        anyhow::bail!("目录不存在或不是目录: {}", root.display());
    }

    // 关键词里带路径分隔符时，用户显然想按路径找，自动切换
    let full_path = args.full_path || args.keyword.contains(['/', '\\']);
    let matcher = Matcher::build(&args.keyword, args.case_sensitive, args.regex)?;

    let exts: Vec<String> = args
        .exts
        .iter()
        .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .collect();

    let cfg = Cfg {
        full_path,
        files_only: args.files_only,
        dirs_only: args.dirs_only,
        exts,
        json: args.json,
        long: args.long,
        absolute: args.absolute,
        max_results: args.max_results,
        sort: args.sort,
        quiet: args.quiet,
    };

    let shared = Arc::new(Shared {
        matcher,
        root: root.clone(),
        cfg,
        scanned: AtomicU64::new(0),
        errors: AtomicU64::new(0),
        matched_files: AtomicU64::new(0),
        matched_dirs: AtomicU64::new(0),
        printed: AtomicUsize::new(0),
        stop: AtomicBool::new(false),
        out: Out::new(),
        collected: Mutex::new(Vec::new()),
    });

    let mut builder = WalkBuilder::new(&root);
    builder
        .hidden(!args.hidden)
        .follow_links(false)
        .threads(args.threads.unwrap_or(0))
        .max_depth(args.max_depth);

    if args.no_ignore {
        builder
            .parents(false)
            .ignore(false)
            .git_global(false)
            .git_ignore(false)
            .git_exclude(false)
            .require_git(false);
    }

    if !args.exclude.is_empty() {
        let mut overrides = OverrideBuilder::new(&root);
        for pat in &args.exclude {
            // Override 里 "!" 前缀表示「排除」，与 gitignore 语义一致
            overrides
                .add(&format!("!{pat}"))
                .with_context(|| format!("排除模式无效: {pat}"))?;
        }
        builder.overrides(overrides.build().context("构建排除规则失败")?);
    }

    let walker = builder.build_parallel();
    walker.run(|| {
        let worker_shared = Arc::clone(&shared);
        let mut batch = Batch::new(Arc::clone(&shared));
        Box::new(move |res| handle_entry(res, &worker_shared, &mut batch))
    });

    // 排序模式：遍历期间只收集，结束后统一排序输出
    if shared.cfg.sort {
        let mut items = {
            let mut guard = shared.collected.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::take(&mut *guard)
        };
        if items.len() >= COLLECT_CAP && !shared.cfg.quiet {
            eprintln!("fsearch: 结果过多，已截断到 {COLLECT_CAP} 条（可加 -n 限制）");
        }
        items.sort_unstable_by(|a, b| a.sort_key.cmp(&b.sort_key));

        let limit = if shared.cfg.max_results == 0 {
            items.len()
        } else {
            shared.cfg.max_results.min(items.len())
        };

        let root = &shared.root;
        let mut buf = Vec::with_capacity(FLUSH_AT);
        for item in items.iter().take(limit) {
            let display = display_path(&item.path, root, shared.cfg.absolute);
            push_entry(&mut buf, &item.path, display, item.is_dir, &shared.cfg);
            if buf.len() >= FLUSH_AT {
                shared.out.write(&buf);
                buf.clear();
            }
        }
        shared.out.write(&buf);
    }

    shared.out.flush();

    if !shared.cfg.quiet {
        print_summary(&shared, started.elapsed().as_secs_f64(), args.threads);
    }
    Ok(())
}

/// 处理并行 walker 交来的单个条目。
fn handle_entry(
    res: Result<ignore::DirEntry, ignore::Error>,
    shared: &Shared,
    batch: &mut Batch,
) -> WalkState {
    if shared.stop.load(Ordering::Relaxed) {
        return WalkState::Quit;
    }

    let entry = match res {
        Ok(entry) => entry,
        // 权限不足、目录被占用等：计数后跳过，不打断整体遍历
        Err(_) => {
            shared.errors.fetch_add(1, Ordering::Relaxed);
            return WalkState::Continue;
        }
    };

    // depth 0 是起始目录本身，不参与匹配
    if entry.depth() == 0 {
        return WalkState::Continue;
    }
    shared.scanned.fetch_add(1, Ordering::Relaxed);

    let cfg = &shared.cfg;
    let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);

    if (cfg.files_only && is_dir) || (cfg.dirs_only && !is_dir) {
        return WalkState::Continue;
    }

    let path = entry.path();
    let rel = path.strip_prefix(&shared.root).unwrap_or(path);

    // 扩展名过滤只针对文件；目录需要继续下钻，不能 Skip
    if !cfg.exts.is_empty() {
        if is_dir {
            return WalkState::Continue;
        }
        let Some(ext) = rel.extension().and_then(|e| e.to_str()) else {
            return WalkState::Continue;
        };
        if !cfg.exts.iter().any(|want| want.eq_ignore_ascii_case(ext)) {
            return WalkState::Continue;
        }
    }

    if !shared
        .matcher
        .is_match(entry.file_name(), rel, cfg.full_path)
    {
        return WalkState::Continue;
    }

    if cfg.sort {
        let mut guard = shared.collected.lock().unwrap_or_else(|e| e.into_inner());
        if guard.len() < COLLECT_CAP {
            guard.push(Item {
                path: path.to_path_buf(),
                is_dir,
                sort_key: rel.as_os_str().as_encoded_bytes().to_vec(),
            });
            if is_dir {
                shared.matched_dirs.fetch_add(1, Ordering::Relaxed);
            } else {
                shared.matched_files.fetch_add(1, Ordering::Relaxed);
            }
        }
        return WalkState::Continue;
    }

    if !shared.reserve_slot() {
        return WalkState::Quit;
    }
    if is_dir {
        shared.matched_dirs.fetch_add(1, Ordering::Relaxed);
    } else {
        shared.matched_files.fetch_add(1, Ordering::Relaxed);
    }

    let display = display_path(path, &shared.root, cfg.absolute);
    push_entry(&mut batch.buf, path, display, is_dir, cfg);
    if batch.buf.len() >= FLUSH_AT {
        batch.flush();
    }
    WalkState::Continue
}

fn display_path<'a>(abs: &'a Path, root: &Path, absolute: bool) -> &'a Path {
    if absolute {
        abs
    } else {
        abs.strip_prefix(root).unwrap_or(abs)
    }
}

/// 把一条命中渲染进缓冲区：普通文本、`--long` 或 JSON Lines。
fn push_entry(buf: &mut Vec<u8>, abs: &Path, display: &Path, is_dir: bool, cfg: &Cfg) {
    if cfg.json {
        let meta = fs::symlink_metadata(abs).ok();
        buf.extend_from_slice(b"{\"path\":");
        json_escape_into(&display.to_string_lossy(), buf);
        buf.extend_from_slice(b",\"kind\":");
        buf.extend_from_slice(if is_dir { b"\"dir\"" } else { b"\"file\"" });
        if let Some(meta) = meta
            && !is_dir
        {
            let _ = write!(buf, ",\"size\":{}", meta.len());
        }
        buf.extend_from_slice(b"}\n");
        return;
    }

    if cfg.long {
        if is_dir {
            buf.extend_from_slice(b"<DIR>     \t");
        } else {
            let size = fs::symlink_metadata(abs).map(|m| m.len()).unwrap_or(0);
            let text = human_size(size);
            for _ in text.len()..10 {
                buf.push(b' ');
            }
            buf.extend_from_slice(text.as_bytes());
            buf.push(b'\t');
        }
    }

    buf.extend_from_slice(display.as_os_str().as_encoded_bytes());
    if is_dir {
        buf.push(std::path::MAIN_SEPARATOR as u8);
    }
    buf.push(b'\n');
}

fn json_escape_into(s: &str, out: &mut Vec<u8>) {
    out.push(b'"');
    for &b in s.as_bytes() {
        match b {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x00..=0x1f => {
                let _ = write!(out, "\\u{b:04x}");
            }
            _ => out.push(b),
        }
    }
    out.push(b'"');
}

fn human_size(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    if n < 1024 {
        return format!("{n}B");
    }
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 10.0 {
        format!("{value:.0}{}", UNITS[unit])
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn print_summary(shared: &Shared, secs: f64, configured_threads: Option<usize>) {
    let files = shared.matched_files.load(Ordering::Relaxed);
    let dirs = shared.matched_dirs.load(Ordering::Relaxed);
    let scanned = shared.scanned.load(Ordering::Relaxed);
    let errors = shared.errors.load(Ordering::Relaxed);
    let threads = configured_threads.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    });

    let mut line = format!(
        "匹配 {} 项（文件 {} · 目录 {}）｜遍历 {} 项｜{:.3}s｜{} 线程",
        thousands(files + dirs),
        thousands(files),
        thousands(dirs),
        thousands(scanned),
        secs,
        threads
    );
    if errors > 0 {
        let _ = write!(line, "｜跳过 {} 个无权限/读取失败项", thousands(errors));
    }
    if shared.cfg.sort {
        let _ = write!(line, "｜已排序");
    }
    eprintln!("{line}");
}
