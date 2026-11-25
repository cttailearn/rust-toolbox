//! 关键词匹配器：默认「不区分大小写的子串」，也可切换为精确匹配 / 正则。
//!
//! 性能要点（这是本工具快的关键之一）：
//! - 纯 ASCII 关键词：`memchr2` 定位首字节大小写两种形态，再用
//!   `eq_ignore_ascii_case` 校验，全程零分配；
//! - 中文等「没有大小写概念」的文字：大小写不敏感 == 精确匹配，
//!   直接走 SIMD 化的 `memmem::find`，同样零分配；
//! - 只有关键词里真的含非 ASCII 大小写字母时，才退到 Unicode 折叠，
//!   而且先用一次精确匹配做快速筛选，命中就不必再做 lowercase。

use std::borrow::Cow;
use std::ffi::OsStr;
use std::path::Path;

use memchr::{memchr2, memmem};
use regex::{Regex, RegexBuilder};

/// 匹配策略。
pub enum Matcher {
    Substr(Substr),
    Regex(Regex),
}

impl Matcher {
    /// 根据参数构建匹配器；正则写错时返回可读的错误。
    pub fn build(keyword: &str, case_sensitive: bool, regex: bool) -> anyhow::Result<Self> {
        if regex {
            let re = RegexBuilder::new(keyword)
                .case_insensitive(!case_sensitive)
                // 限制正则体积，避免恶意/误写模式占用过多内存
                .size_limit(16 << 20)
                .build()
                .map_err(|e| anyhow::anyhow!("正则表达式无效: {e}"))?;
            return Ok(Self::Regex(re));
        }
        Ok(Self::Substr(Substr::new(keyword, case_sensitive)))
    }

    /// `name` 为文件名，`rel` 为相对路径；`full_path` 决定拿哪个去匹配。
    ///
    /// 路径匹配时先按原生分隔符比一次；不中且路径含 `\`（Windows），
    /// 再把 `\` 换成 `/` 重试，这样 `src/main` 和 `src\main` 两种写法都能用。
    pub fn is_match(&self, name: &OsStr, rel: &Path, full_path: bool) -> bool {
        match self {
            Self::Substr(s) => {
                if full_path {
                    if s.matches(rel.as_os_str().as_encoded_bytes(), || rel.to_string_lossy()) {
                        return true;
                    }
                    return match normalize_path(rel) {
                        Some(text) => s.matches(text.as_bytes(), || Cow::Borrowed(text.as_str())),
                        None => false,
                    };
                }
                s.matches(name.as_encoded_bytes(), || name.to_string_lossy())
            }
            Self::Regex(re) => {
                let hay = if full_path {
                    rel.to_string_lossy()
                } else {
                    name.to_string_lossy()
                };
                if re.is_match(&hay) {
                    return true;
                }
                if full_path
                    && let Some(text) = normalize_path(rel) {
                        return re.is_match(&text);
                    }
                false
            }
        }
    }
}

/// 把 Windows 反斜杠路径转成 `/`，便于跨平台书写关键词；无 `\` 时返回 None。
fn normalize_path(p: &Path) -> Option<String> {
    let text = p.to_string_lossy();
    if !text.contains('\\') {
        return None;
    }
    Some(text.replace('\\', "/"))
}

/// 子串匹配器，按关键词形态预先选定最快的实现路径。
pub struct Substr {
    needle: Vec<u8>,
    needle_lower: String,
    mode: Mode,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// 字节精确匹配
    Exact,
    /// ASCII 大小写不敏感
    AsciiCi,
    /// Unicode 大小写不敏感（中文、emoji 等不会走到这里）
    UnicodeCi,
}

impl Substr {
    fn new(keyword: &str, case_sensitive: bool) -> Self {
        let mode = if case_sensitive {
            Mode::Exact
        } else if keyword.is_ascii() {
            Mode::AsciiCi
        } else if keyword
            .chars()
            .any(|c| c.is_uppercase() || c.is_lowercase())
        {
            Mode::UnicodeCi
        } else {
            // 关键词里没有任何「有大小写」的字符（典型：中文），
            // 大小写折叠不可能产生它，因此精确匹配完全等价。
            Mode::Exact
        };
        Self {
            needle: keyword.as_bytes().to_vec(),
            needle_lower: keyword.to_lowercase(),
            mode,
        }
    }

    /// `hay` 是目标字节；只有 Unicode 折叠路径才会用到 `to_text`（惰性求值）。
    fn matches<'a, F>(&self, hay: &[u8], to_text: F) -> bool
    where
        F: FnOnce() -> Cow<'a, str>,
    {
        match self.mode {
            Mode::Exact => memmem::find(hay, &self.needle).is_some(),
            Mode::AsciiCi => find_ascii_ci(hay, &self.needle),
            Mode::UnicodeCi => {
                if memmem::find(hay, &self.needle).is_some() {
                    return true;
                }
                to_text().to_lowercase().contains(&self.needle_lower)
            }
        }
    }
}

/// ASCII 大小写不敏感的子串查找。
///
/// 先按首字母的两种大小写形态定位候选位置（memchr2 是 SIMD 的），
/// 再用 `eq_ignore_ascii_case` 校验整段，避免逐字符 lower 带来的分配。
fn find_ascii_ci(hay: &[u8], needle: &[u8]) -> bool {
    let n = needle.len();
    if n == 0 {
        return true;
    }
    if n > hay.len() {
        return false;
    }
    let last_start = hay.len() - n;
    let lower = needle[0].to_ascii_lowercase();
    let upper = needle[0].to_ascii_uppercase();

    let mut start = 0;
    while start <= last_start {
        let Some(off) = memchr2(lower, upper, &hay[start..=last_start]) else {
            return false;
        };
        let pos = start + off;
        if hay[pos..pos + n].eq_ignore_ascii_case(needle) {
            return true;
        }
        start = pos + 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(kw: &str, cs: bool, regex: bool) -> Matcher {
        Matcher::build(kw, cs, regex).unwrap()
    }

    fn name(s: &str) -> &OsStr {
        OsStr::new(s)
    }

    #[test]
    fn ascii_ci_matches() {
        let mat = m("ReadMe", false, false);
        assert!(mat.is_match(name("readme.md"), Path::new("x"), false));
        assert!(mat.is_match(name("README.MD"), Path::new("x"), false));
        assert!(!mat.is_match(name("read.me"), Path::new("x"), false));
    }

    #[test]
    fn ascii_exact() {
        let mat = m("ReadMe", true, false);
        assert!(mat.is_match(name("ReadMe.md"), Path::new("x"), false));
        assert!(!mat.is_match(name("readme.md"), Path::new("x"), false));
    }

    #[test]
    fn chinese_zero_case() {
        let mat = m("报告", false, false);
        assert!(mat.is_match(name("2024年度报告.docx"), Path::new("x"), false));
        assert!(!mat.is_match(name("2024年度总结.docx"), Path::new("x"), false));
    }

    #[test]
    fn unicode_ci() {
        let mat = m("Ärger", false, false);
        assert!(mat.is_match(name("ärger.txt"), Path::new("x"), false));
        assert!(!mat.is_match(name("argern.txt"), Path::new("x"), false));
    }

    #[test]
    fn regex_mode() {
        let mat = m(r"^20\d\d-", false, true);
        assert!(mat.is_match(name("2024-01-报告.md"), Path::new("x"), false));
        assert!(!mat.is_match(name("x2024-01"), Path::new("x"), false));
    }

    #[test]
    fn full_path_mode() {
        let mat = m("src/main", false, false);
        assert!(mat.is_match(name("main.rs"), Path::new(r"proj\src\main.rs"), true));
        assert!(!mat.is_match(name("main.rs"), Path::new(r"proj\tests\main.rs"), true));
    }

    #[test]
    fn empty_needle_matches_everything() {
        let mat = m("", false, false);
        assert!(mat.is_match(name("anything"), Path::new("x"), false));
    }
}
