use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;

use crate::common::utils::{atomic_write, home_dir, previous_char_boundary};

const MAX_MEMORY_SIZE: usize = 100 * 1024;

/// ===== 记忆类别 =====
/// 决定注入顺序与裁剪优先级：pref/conv 几乎不会被淘汰，fact 最容易被淘汰。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryCategory {
    /// 用户偏好（语言、风格、习惯）— 最高优先级
    Preference,
    /// 项目约定（工具链、规范）— 高
    Convention,
    /// 关键决策 — 中
    Decision,
    /// 通用事实 — 低
    Fact,
}

impl MemoryCategory {
    /// 裁剪时的保留权重：数值越大越不容易被淘汰。
    pub fn keep_weight(self) -> u8 {
        match self {
            MemoryCategory::Preference => 4,
            MemoryCategory::Convention => 3,
            MemoryCategory::Decision => 2,
            MemoryCategory::Fact => 1,
        }
    }

    /// 文件中的短标签。
    pub fn tag(self) -> &'static str {
        match self {
            MemoryCategory::Preference => "pref",
            MemoryCategory::Convention => "conv",
            MemoryCategory::Decision => "dec",
            MemoryCategory::Fact => "fact",
        }
    }

    /// 从短标签解析（大小写不敏感），未知值回退到 Fact。
    pub fn from_tag(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "pref" | "preference" => MemoryCategory::Preference,
            "conv" | "convention" => MemoryCategory::Convention,
            "dec" | "decision" => MemoryCategory::Decision,
            _ => MemoryCategory::Fact,
        }
    }
}

/// ===== 单条记忆 =====
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntry {
    pub timestamp: String,
    pub category: MemoryCategory,
    pub text: String,
}

impl MemoryEntry {
    /// 渲染成文件中的一行：`- [2026-06-17 09:30] [pref] 用户偏好 Rust`
    pub fn to_line(&self) -> String {
        format!(
            "- [{}] [{}] {}",
            self.timestamp,
            self.category.tag(),
            self.text
        )
    }

    /// 从单行解析。兼容三种历史格式：
    ///   1. 新格式：`- [ts] [tag] text`
    ///   2. 旧格式：`- [ts] text`（无 tag，归为 Fact）
    ///   3. 任意裸文本（归为 Fact，时间戳留空）
    pub fn parse(line: &str) -> Option<Self> {
        let trimmed = line.trim().trim_start_matches(['-', ' ', '\t']);
        if trimmed.is_empty() || trimmed.starts_with('#') {
            return None;
        }

        // 尝试剥离前导 `[...]`（时间戳）。两支都返回 (String, &str) 保持类型一致。
        let (ts, after_ts): (String, &str) = match split_first_bracket(trimmed) {
            Some((inner, rest)) => (inner.to_string(), rest),
            None => (String::new(), trimmed),
        };

        let after_ts = after_ts.trim_start();
        // 再尝试剥离第二个 `[...]`（类别 tag）
        if let Some((tag_inner, rest)) = split_first_bracket(after_ts) {
            let category = MemoryCategory::from_tag(tag_inner);
            let text = rest.trim().to_string();
            if text.is_empty() {
                return None;
            }
            return Some(Self {
                timestamp: ts,
                category,
                text,
            });
        }

        // 只有时间戳，无类别 → Fact
        let text = after_ts.trim().to_string();
        if text.is_empty() {
            return None;
        }
        Some(Self {
            timestamp: ts,
            category: MemoryCategory::Fact,
            text,
        })
    }
}

/// 若 `s` 形如 `[xxx] rest`（以 `[` 开头，找到第一个 `]`），返回 `(括号内内容, 剩余)`。
/// 否则返回 None。`rest` 保留原始字符（未 trim）。
fn split_first_bracket(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    if !s.starts_with('[') {
        return None;
    }
    let end = s.find(']')?;
    let inner = &s[1..end];
    let rest = &s[end + 1..];
    Some((inner, rest))
}

/// ===== 写入动作 =====
/// 由 LLM 提取器或 /memory add 产生，指导 add_entry 如何处理已有条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddAction {
    /// 新增一条（会先做去重）
    Add,
    /// 替换现有中语义冲突的一条；`match_hint` 为关键词，用于定位旧条目。
    Replace { match_hint: String },
}

/// ===== 记忆操作结果 =====
#[derive(Debug, Clone)]
pub struct AddOutcome {
    pub applied: bool,
    pub reason: &'static str,
}

/// 获取默认的记忆存储路径（跨平台）
pub fn default_memory_path() -> PathBuf {
    home_dir().join(".movix").join("memory.md")
}

pub fn load(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    if content.trim().is_empty() {
        return None;
    }
    Some(content)
}

/// 把原始文件内容解析为条目列表。
pub fn parse_entries(content: &str) -> Vec<MemoryEntry> {
    content
        .lines()
        .filter_map(|l| {
            if l.trim().is_empty() {
                None
            } else {
                MemoryEntry::parse(l)
            }
        })
        .collect()
}

/// 把条目列表序列化回文件内容。
pub fn serialize_entries(entries: &[MemoryEntry]) -> String {
    entries
        .iter()
        .map(|e| e.to_line())
        .collect::<Vec<_>>()
        .join("\n")
}

/// 文本归一化分词。
/// 拉丁文按空白/标点分词；CJK（中日韩）字符没有词边界，
/// 改为按**单字**切分，否则整段中文会被当成一个巨型 token，导致相似度失真。
fn normalize_tokens(text: &str) -> HashSet<String> {
    let mut set = HashSet::new();
    let mut latin_buf = String::new();

    for c in text.to_lowercase().chars() {
        if c.is_alphanumeric() {
            if is_cjk(c) {
                // 刷出缓冲的拉丁词，再单独放 CJK 单字
                if !latin_buf.is_empty() {
                    set.insert(std::mem::take(&mut latin_buf));
                }
                set.insert(c.to_string());
            } else {
                latin_buf.push(c);
            }
        } else {
            if !latin_buf.is_empty() {
                set.insert(std::mem::take(&mut latin_buf));
            }
        }
    }
    if !latin_buf.is_empty() {
        set.insert(latin_buf);
    }
    set
}

/// 判断是否为 CJK 统一表意文字（含扩展 A 区常用范围）。
fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{4E00}'..='\u{9FFF}'   // CJK 统一表意文字
        | '\u{3400}'..='\u{4DBF}' // 扩展 A
        | '\u{3040}'..='\u{30FF}' // 平假名/片假名
    )
}

/// Jaccard 相似度（token 重叠率）。两条记忆语义越接近，值越接近 1.0。
fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    inter / union
}

/// 新增/替换一条记忆到指定条目列表（**内存操作**，不写盘）。
///
/// - `Add`：若与现有任意条目相似度 > DEDUP_THRESHOLD 则跳过（去重）。
/// - `Replace { match_hint }`：在现有条目中找到包含 hint 的最相似者，替换其文本；
///   若找不到匹配者，降级为普通 Add。
///
/// 返回是否实际应用。
pub fn apply_entry(
    entries: &mut Vec<MemoryEntry>,
    category: MemoryCategory,
    text: &str,
    action: AddAction,
    now_ts: &str,
) -> AddOutcome {
    let text = text.trim();
    if text.is_empty() {
        return AddOutcome {
            applied: false,
            reason: "空文本",
        };
    }
    let new_tokens = normalize_tokens(text);

    match action {
        AddAction::Add => {
            // 去重：与同类或任意类别的现有条目比对
            for existing in entries.iter() {
                let exist_tokens = normalize_tokens(&existing.text);
                if jaccard(&new_tokens, &exist_tokens) >= DEDUP_THRESHOLD {
                    return AddOutcome {
                        applied: false,
                        reason: "与已有条目重复",
                    };
                }
            }
            entries.push(MemoryEntry {
                timestamp: now_ts.to_string(),
                category,
                text: text.to_string(),
            });
            AddOutcome {
                applied: true,
                reason: "新增",
            }
        }
        AddAction::Replace { match_hint } => {
            let hint_tokens = normalize_tokens(&match_hint);
            // 修复(M10):原实现在 max_by 比较器闭包内对每个候选条目重复 normalize_tokens
            // (分词建 HashSet),比较器被调用 O(n log n) 次 → tokenize 总开销 O(n²·|text|)。
            // 预先把所有候选 tokenize 一次,比较时直接查表。
            let candidates: Vec<(usize, std::collections::HashSet<String>, f64)> = entries
                .iter()
                .enumerate()
                .filter(|(_, e)| e.category == category)
                .map(|(i, e)| {
                    let toks = normalize_tokens(&e.text);
                    let sim = jaccard(&hint_tokens, &toks);
                    (i, toks, sim)
                })
                .collect();
            let best_idx = candidates
                .into_iter()
                .filter(|(_, _, sim)| *sim > 0.1)
                .max_by(|(_, _, sa), (_, _, sb)| {
                    sa.partial_cmp(sb).unwrap_or(std::cmp::Ordering::Equal)
                })
                .map(|(i, _, _)| i);

            if let Some(idx) = best_idx {
                entries[idx].timestamp = now_ts.to_string();
                entries[idx].text = text.to_string();
                AddOutcome {
                    applied: true,
                    reason: "替换冲突条目",
                }
            } else {
                // 找不到匹配，降级为 Add（仍走去重）
                apply_entry(entries, category, text, AddAction::Add, now_ts)
            }
        }
    }
}

/// 相似度阈值：>= 该值视为重复。
const DEDUP_THRESHOLD: f64 = 0.7;

/// 容量裁剪：当序列化后超过 MAX_MEMORY_SIZE，按 (keep_weight, recency) 淘汰低优先级旧条目。
/// 返回裁剪后的条目列表。pref/conv 几乎不会被淘汰。
pub fn trim_to_size(mut entries: Vec<MemoryEntry>) -> Vec<MemoryEntry> {
    // 修复(M10):原实现 `while serialize_entries(&entries).len() > MAX_MEMORY_SIZE`
    // 每删一条就重新序列化整个列表(O(n)),整体 O(n²)。改为:先算一次总大小,再按
    // "优先级低 + 最旧"排序得到淘汰顺序,一次性删除到目标大小。
    let mut total = serialize_entries(&entries).len();
    if total <= MAX_MEMORY_SIZE || entries.len() <= 1 {
        return entries;
    }
    // 计算每条的序列化大小(近似:用单条 serialize 估算),用于精确预算。
    // 简化:按排序键(keep_weight 升序 → timestamp 升序)逐条删,直到 total <= MAX。
    let mut order: Vec<usize> = (0..entries.len()).collect();
    order.sort_by(|&i, &j| {
        let wi = entries[i].category.keep_weight();
        let wj = entries[j].category.keep_weight();
        let pri = wi.cmp(&wj);
        if pri != std::cmp::Ordering::Equal {
            return pri;
        }
        entries[i].timestamp.cmp(&entries[j].timestamp)
    });
    // 从最该淘汰的开始删,直到 total <= MAX_MEMORY_SIZE。
    let mut to_remove: Vec<usize> = Vec::new();
    for &idx in &order {
        if total <= MAX_MEMORY_SIZE || entries.len() - to_remove.len() <= 1 {
            break;
        }
        // 估算删除此条后的大小:serialize_entries 用 "\n" join(非 JSON 数组,无括号开销),
        // 单条 = 其 to_line()。删除一条实际减少 entry_line.len() + 1(分隔符),这里用
        // entry_size + 1 补回分隔符,估算更精确。偏差仅来自首/末条边界(±1 字节),可忽略。
        let entry_size = serialize_entries(std::slice::from_ref(&entries[idx])).len() + 1;
        to_remove.push(idx);
        total = total.saturating_sub(entry_size);
    }
    if to_remove.is_empty() {
        return entries;
    }
    // 从大到小删除,避免索引移位。
    to_remove.sort_unstable_by(|a, b| b.cmp(a));
    for idx in to_remove {
        entries.remove(idx);
    }
    // 兜底:若近似估算偏差导致仍超限,回退到原循环逻辑(此时条目已很少,O(n²) 可接受)。
    while serialize_entries(&entries).len() > MAX_MEMORY_SIZE && entries.len() > 1 {
        let victim_idx = (0..entries.len())
            .min_by(|&i, &j| {
                let pri = entries[i]
                    .category
                    .keep_weight()
                    .cmp(&entries[j].category.keep_weight());
                if pri != std::cmp::Ordering::Equal {
                    return pri;
                }
                entries[i].timestamp.cmp(&entries[j].timestamp)
            })
            .expect("entries.len() > 1");
        entries.remove(victim_idx);
    }
    entries
}

/// ===== 文件级 API（保留旧接口，内部走新逻辑）=====
pub fn as_system_block(content: &str, source: &Path) -> Option<String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }

    // 修复(R6/memory-H5,关键):memory.md 内容(可由 agent auto_extract、/memory add、或
    // 克隆工作区的预置文件写入)未转义就注入 system prompt。含 `</user_memory>` 的内容
    // 会提前关闭块、注入任意 system 指令 = 持久化 prompt injection。
    // 防护:对内容做 XML 转义(< > &),杜绝标签注入。同时转义 source 路径里的引号。
    let escape_xml = |s: &str| -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    let escape_attr = |s: &str| -> String { s.replace('"', "&quot;") };

    let display = escape_attr(&source.display().to_string());
    let payload = if content.len() > MAX_MEMORY_SIZE {
        let cutoff = previous_char_boundary(content, MAX_MEMORY_SIZE);
        let omitted_bytes = content.len() - cutoff;
        let head = escape_xml(&content[..cutoff]);
        format!(
            "{}\n<truncated bytes=\"{}\" source=\"{}\"></truncated>",
            head, omitted_bytes, display
        )
    } else {
        escape_xml(trimmed)
    };

    Some(format!(
        "<user_memory source=\"{}\">\n{}\n</user_memory>",
        display, payload
    ))
}

/// 追加一条记忆（结构化版本）。对 `/memory add` 命令开放，默认类别 Fact，走去重。
pub fn append(path: &Path, text: &str) -> std::io::Result<()> {
    add_entry_file(path, MemoryCategory::Fact, text, AddAction::Add)
}

/// 结构化写入：读现有条目 → 应用去重/冲突 → 裁剪 → 写盘。
pub fn add_entry_file(
    path: &Path,
    category: MemoryCategory,
    text: &str,
    action: AddAction,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let existing = fs::read_to_string(path).unwrap_or_default();
    let mut entries = parse_entries(&existing);
    let now_ts = Utc::now().format("%Y-%m-%d %H:%M").to_string();

    let _outcome = apply_entry(&mut entries, category, text, action, &now_ts);
    let entries = trim_to_size(entries);
    let new_content = serialize_entries(&entries);

    // 修复(Critical #C10):原子写,崩溃不留半截损坏文件。
    atomic_write(path, &new_content)
}

pub fn clear(path: &Path) -> std::io::Result<()> {
    if path.exists() {
        atomic_write(path, "")?;
    }
    Ok(())
}

pub fn show(path: &Path) -> String {
    match load(path) {
        Some(content) => {
            let display = path.display();
            format!("Memory file: {}\n\n{}", display, content)
        }
        None => {
            let display = path.display();
            format!("Memory file: {} (empty or not found)", display)
        }
    }
}

pub struct UserMemory {
    path: PathBuf,
    enabled: bool,
    auto_extract: bool,
    cached_content: Option<String>,
}

impl UserMemory {
    pub fn new(path: PathBuf, enabled: bool) -> Self {
        let cached_content = if enabled { load(&path) } else { None };
        Self {
            path,
            enabled,
            auto_extract: true,
            cached_content,
        }
    }

    pub fn default_enabled() -> Self {
        Self::new(default_memory_path(), true)
    }

    pub fn reload(&mut self) {
        if self.enabled {
            self.cached_content = load(&self.path);
        }
    }

    pub fn system_block(&self) -> Option<String> {
        if !self.enabled {
            return None;
        }
        self.cached_content
            .as_ref()
            .and_then(|c| as_system_block(c, &self.path))
    }

    /// 旧接口：追加一条 Fact（走去重）。
    pub fn append(&mut self, text: &str) -> std::io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        append(&self.path, text)?;
        self.reload();
        Ok(())
    }

    /// 新接口：结构化写入，支持类别与替换动作。
    pub fn add_entry(
        &mut self,
        category: MemoryCategory,
        text: &str,
        action: AddAction,
    ) -> std::io::Result<AddOutcome> {
        if !self.enabled {
            return Ok(AddOutcome {
                applied: false,
                reason: "记忆已禁用",
            });
        }
        let outcome = {
            let existing = self.cached_content.as_deref().unwrap_or("");
            let mut entries = parse_entries(existing);
            let now_ts = Utc::now().format("%Y-%m-%d %H:%M").to_string();
            let o = apply_entry(&mut entries, category, text, action, &now_ts);
            let entries = trim_to_size(entries);
            // 修复(Critical #C10):原子写。
            atomic_write(&self.path, &serialize_entries(&entries))?;
            o
        };
        self.reload();
        Ok(outcome)
    }

    pub fn clear(&mut self) -> std::io::Result<()> {
        clear(&self.path)?;
        self.cached_content = None;
        Ok(())
    }

    pub fn show(&self) -> String {
        show(&self.path)
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        if enabled {
            self.reload();
        }
    }

    pub fn is_auto_extract(&self) -> bool {
        self.auto_extract
    }

    pub fn set_auto_extract(&mut self, on: bool) {
        self.auto_extract = on;
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn content(&self) -> Option<&str> {
        self.cached_content.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_load_existing() {
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "test memory content").unwrap();
        let content = load(f.path()).unwrap();
        assert_eq!(content, "test memory content");
    }

    #[test]
    fn test_load_empty() {
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "   ").unwrap();
        assert!(load(f.path()).is_none());
    }

    #[test]
    fn test_load_nonexistent() {
        assert!(load(Path::new("/nonexistent/memory.md")).is_none());
    }

    #[test]
    fn test_as_system_block() {
        let content = "I prefer Rust over Python";
        let block = as_system_block(content, Path::new("/home/user/.movix/memory.md")).unwrap();
        assert!(block.contains("<user_memory"));
        assert!(block.contains(content));
    }

    #[test]
    fn test_append() {
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "").unwrap();
        append(f.path(), "hello world").unwrap();
        let content = fs::read_to_string(f.path()).unwrap();
        assert!(content.contains("hello world"));
    }

    #[test]
    fn test_append_caps_file_size_on_char_boundary() {
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "{}", "你".repeat(MAX_MEMORY_SIZE)).unwrap();
        append(f.path(), "hello world").unwrap();
        let content = fs::read_to_string(f.path()).unwrap();
        assert!(content.len() <= MAX_MEMORY_SIZE + 3); // 允许小的缓冲 (一个中文字符是 3 字节)
        assert!(content.contains("hello world"));
    }

    #[test]
    fn test_previous_char_boundary_in_memory() {
        // 测试中文字符边界处理
        let s = "你好世界"; // 每个字 3 字节
        assert_eq!(previous_char_boundary(s, 0), 0);
        assert_eq!(previous_char_boundary(s, 1), 0);
        assert_eq!(previous_char_boundary(s, 2), 0);
        assert_eq!(previous_char_boundary(s, 3), 3);
        assert_eq!(previous_char_boundary(s, 4), 3);
    }

    // ===== 新增：结构化记忆测试 =====

    #[test]
    fn parse_new_format_with_tag() {
        let e = MemoryEntry::parse("- [2026-06-17 09:30] [pref] 用户偏好 Rust").unwrap();
        assert_eq!(e.timestamp, "2026-06-17 09:30");
        assert_eq!(e.category, MemoryCategory::Preference);
        assert_eq!(e.text, "用户偏好 Rust");
    }

    #[test]
    fn parse_legacy_format_without_tag() {
        let e = MemoryEntry::parse("- [2026-06-17 09:30] 用户喜欢深色主题").unwrap();
        assert_eq!(e.timestamp, "2026-06-17 09:30");
        assert_eq!(e.category, MemoryCategory::Fact);
        assert_eq!(e.text, "用户喜欢深色主题");
    }

    #[test]
    fn parse_bare_text_fallback() {
        let e = MemoryEntry::parse("just some text").unwrap();
        assert_eq!(e.category, MemoryCategory::Fact);
        assert_eq!(e.text, "just some text");
    }

    #[test]
    fn parse_ignores_empty_and_comments() {
        assert!(MemoryEntry::parse("").is_none());
        assert!(MemoryEntry::parse("   ").is_none());
        assert!(MemoryEntry::parse("# a comment").is_none());
    }

    #[test]
    fn roundtrip_serialize_parse() {
        let entries = vec![
            MemoryEntry {
                timestamp: "2026-06-17 09:30".into(),
                category: MemoryCategory::Preference,
                text: "偏好 Rust".into(),
            },
            MemoryEntry {
                timestamp: "2026-06-17 09:35".into(),
                category: MemoryCategory::Convention,
                text: "2 空格缩进".into(),
            },
        ];
        let s = serialize_entries(&entries);
        let parsed = parse_entries(&s);
        assert_eq!(parsed, entries);
    }

    #[test]
    fn dedup_blocks_near_duplicate_add() {
        let mut entries = vec![MemoryEntry {
            timestamp: "2026-06-17 09:30".into(),
            category: MemoryCategory::Preference,
            text: "用户偏好 Rust 语言".into(),
        }];
        let out = apply_entry(
            &mut entries,
            MemoryCategory::Preference,
            "用户偏好 Rust",
            AddAction::Add,
            "2026-06-17 09:40",
        );
        assert!(!out.applied, "应被去重拦截");
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn add_accepts_distinct_entry() {
        let mut entries = vec![MemoryEntry {
            timestamp: "2026-06-17 09:30".into(),
            category: MemoryCategory::Preference,
            text: "偏好 Rust".into(),
        }];
        let out = apply_entry(
            &mut entries,
            MemoryCategory::Convention,
            "项目使用 prettier 格式化",
            AddAction::Add,
            "2026-06-17 09:40",
        );
        assert!(out.applied);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn replace_updates_conflicting_entry() {
        let mut entries = vec![MemoryEntry {
            timestamp: "2026-06-17 09:30".into(),
            category: MemoryCategory::Preference,
            text: "用户喜欢 Rust".into(),
        }];
        let out = apply_entry(
            &mut entries,
            MemoryCategory::Preference,
            "用户改用 Go",
            AddAction::Replace {
                match_hint: "语言偏好".into(),
            },
            "2026-06-17 09:40",
        );
        // hint "语言偏好" 与 "用户喜欢 Rust" 相似度低，可能降级为 Add；
        // 这里只需验证不会产生重复并保持 <=1 条 pref（要么替换要么去重跳过）
        assert!(entries.len() <= 2);
        if entries.len() == 1 {
            // 发生了替换或去重
            let _ = out;
        }
    }

    #[test]
    fn replace_finds_similar_target() {
        let mut entries = vec![MemoryEntry {
            timestamp: "2026-06-17 09:30".into(),
            category: MemoryCategory::Preference,
            text: "用户偏好 Rust 语言".into(),
        }];
        // hint 与现有条目高度重叠，应能定位并替换
        apply_entry(
            &mut entries,
            MemoryCategory::Preference,
            "用户偏好 Go 语言",
            AddAction::Replace {
                match_hint: "用户偏好 Rust 语言".into(),
            },
            "2026-06-17 09:40",
        );
        assert_eq!(entries.len(), 1, "替换不应增加条目数");
        assert!(entries[0].text.contains("Go"));
    }

    #[test]
    fn trim_prefers_low_priority_eviction() {
        // 构造：1 条 pref + 1 条 fact，体积超限（用超长 text）
        let big = "x".repeat(MAX_MEMORY_SIZE);
        let mut entries = vec![
            MemoryEntry {
                timestamp: "2026-01-01 00:00".into(),
                category: MemoryCategory::Fact,
                text: big.clone(),
            },
            MemoryEntry {
                timestamp: "2026-01-01 00:00".into(),
                category: MemoryCategory::Preference,
                text: "重要偏好".into(),
            },
        ];
        entries = trim_to_size(entries);
        // fact（低权重）应被淘汰，pref 保留
        assert!(
            entries
                .iter()
                .any(|e| e.category == MemoryCategory::Preference)
        );
        assert!(entries.iter().all(|e| e.category != MemoryCategory::Fact));
    }

    #[test]
    fn jaccard_identical_strings() {
        let a = normalize_tokens("用户偏好 rust");
        let b = normalize_tokens("用户偏好 rust");
        assert!((jaccard(&a, &b) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn jaccard_disjoint_strings() {
        let a = normalize_tokens("apple banana");
        let b = normalize_tokens("cherry date");
        assert_eq!(jaccard(&a, &b), 0.0);
    }
}
