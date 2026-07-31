use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const MAX_INDEX_FILE_SIZE: usize = 100_000;
const MAX_INDEX_ENTRIES: usize = 500;

/// 文件类型分类
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileCategory {
    Source,
    Test,
    Config,
    Documentation,
    Asset,
    Build,
    Other,
}

/// 文件索引条目
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    /// 相对于工作区的路径
    pub relative_path: String,
    /// 文件类型分类
    pub category: FileCategory,
    /// 文件大小（字节）
    pub size: u64,
    /// 主要符号列表（函数/类/结构体名）
    pub symbols: Vec<String>,
    /// 导入的模块列表
    pub imports: Vec<String>,
    /// 语言
    pub language: String,
    /// 最后修改时间
    pub modified_at: Option<u64>,
}

/// 项目结构概览
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectIndex {
    /// 工作区路径
    pub workspace: String,
    /// 文件索引
    pub files: Vec<FileEntry>,
    /// 项目语言分布（语言 -> 文件数）
    pub language_distribution: HashMap<String, usize>,
    /// 目录树（简化版）
    pub directory_tree: String,
    /// 项目类型
    pub project_type: ProjectType,
    /// 索引构建时间
    pub indexed_at: String,
    /// 总文件数
    pub total_files: usize,
    /// 总代码行数估计
    pub total_lines_estimate: u64,
}

/// 项目类型
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProjectType {
    Rust,
    Python,
    TypeScript,
    JavaScript,
    Go,
    Java,
    Mixed,
    Unknown,
}

/// 项目索引构建器
pub struct ProjectIndexer {
    workspace: PathBuf,
}

impl ProjectIndexer {
    /// 创建项目索引器
    pub fn new(workspace: &Path) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
        }
    }

    /// 构建项目索引
    pub fn build_index(&self) -> ProjectIndex {
        let mut files = Vec::new();
        let mut language_distribution: HashMap<String, usize> = HashMap::new();
        let mut total_lines: u64 = 0;

        self.walk_and_index(
            &self.workspace,
            &mut files,
            &mut language_distribution,
            &mut total_lines,
        );

        let project_type = self.detect_project_type(&language_distribution);
        let directory_tree = self.build_directory_tree();
        let total_files = files.len();

        let indexed_at = chrono::Utc::now().to_rfc3339();

        ProjectIndex {
            workspace: self.workspace.to_string_lossy().to_string(),
            files,
            language_distribution,
            directory_tree,
            project_type,
            indexed_at,
            total_files,
            total_lines_estimate: total_lines,
        }
    }

    /// 将项目索引格式化为上下文注入块
    pub fn format_for_context(index: &ProjectIndex) -> String {
        let mut parts = Vec::new();

        parts.push(format!(
            "<project_index type=\"{:?}\" files=\"{}\" lines=\"{}\">",
            index.project_type, index.total_files, index.total_lines_estimate
        ));

        parts.push(format!(
            "\n## Directory Structure\n{}",
            index.directory_tree
        ));

        parts.push("\n## Language Distribution".into());
        for (lang, count) in &index.language_distribution {
            parts.push(format!("- {}: {} files", lang, count));
        }

        parts.push("\n## Key Files".into());
        let key_files: Vec<&FileEntry> = index
            .files
            .iter()
            .filter(|f| f.category == FileCategory::Source && !f.symbols.is_empty())
            .take(50)
            .collect();

        for file in key_files {
            if file.symbols.is_empty() {
                parts.push(format!("- {} [{}]", file.relative_path, file.language));
            } else {
                parts.push(format!(
                    "- {} [{}]: {}",
                    file.relative_path,
                    file.language,
                    file.symbols.join(", ")
                ));
            }
        }

        parts.push("</project_index>".into());
        parts.join("\n")
    }

    /// 递归遍历并索引文件
    fn walk_and_index(
        &self,
        dir: &Path,
        files: &mut Vec<FileEntry>,
        lang_dist: &mut HashMap<String, usize>,
        total_lines: &mut u64,
    ) {
        if files.len() >= MAX_INDEX_ENTRIES {
            return;
        }

        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();

                if should_skip_entry(&path) {
                    continue;
                }

                if path.is_dir() {
                    // 修复(Low-index):跳过符号链接目录,防环导致重复 read_to_string 放大。
                    // 原 `path.is_dir()` 跟随符号链接,环场景下重复读同一物理文件直到条目满 500。
                    let is_symlink = entry
                        .path()
                        .symlink_metadata()
                        .map(|m| m.is_symlink())
                        .unwrap_or(false);
                    if !is_symlink {
                        self.walk_and_index(&path, files, lang_dist, total_lines);
                    }
                } else if is_indexable_file(&path)
                    && let Some(entry) = self.index_file(&path)
                {
                    *lang_dist.entry(entry.language.clone()).or_insert(0) += 1;
                    *total_lines += entry.size / 30;
                    files.push(entry);
                }
            }
        }
    }

    /// 索引单个文件
    fn index_file(&self, path: &Path) -> Option<FileEntry> {
        let metadata = std::fs::metadata(path).ok()?;
        if metadata.len() > MAX_INDEX_FILE_SIZE as u64 {
            return None;
        }

        let relative_path = path
            .strip_prefix(&self.workspace)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();

        let language = detect_language(path);
        let category = categorize_file(path);
        let modified_at = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs());

        let content = std::fs::read_to_string(path).unwrap_or_default();
        let symbols = extract_symbols(&content, &language);
        let imports = extract_import_list(&content, &language);

        Some(FileEntry {
            relative_path,
            category,
            size: metadata.len(),
            symbols,
            imports,
            language,
            modified_at,
        })
    }

    /// 检测项目类型
    fn detect_project_type(&self, lang_dist: &HashMap<String, usize>) -> ProjectType {
        let rust_count = *lang_dist.get("Rust").unwrap_or(&0);
        let py_count = *lang_dist.get("Python").unwrap_or(&0);
        let ts_count = *lang_dist.get("TypeScript").unwrap_or(&0);
        let js_count = *lang_dist.get("JavaScript").unwrap_or(&0);
        let go_count = *lang_dist.get("Go").unwrap_or(&0);
        let java_count = *lang_dist.get("Java").unwrap_or(&0);

        let max = rust_count
            .max(py_count)
            .max(ts_count)
            .max(js_count)
            .max(go_count)
            .max(java_count);
        if max == 0 {
            return ProjectType::Unknown;
        }

        let total: usize = lang_dist.values().sum();
        let dominant_ratio = max as f32 / total as f32;

        if dominant_ratio > 0.6 {
            if max == rust_count {
                ProjectType::Rust
            } else if max == py_count {
                ProjectType::Python
            } else if max == ts_count {
                ProjectType::TypeScript
            } else if max == js_count {
                ProjectType::JavaScript
            } else if max == go_count {
                ProjectType::Go
            } else if max == java_count {
                ProjectType::Java
            } else {
                ProjectType::Mixed
            }
        } else {
            ProjectType::Mixed
        }
    }

    /// 构建目录树
    fn build_directory_tree(&self) -> String {
        let mut tree = String::new();
        let mut emitted = 0usize;
        self.build_tree_recursive(&self.workspace, "", &mut tree, 0, &mut emitted);
        tree
    }

    fn build_tree_recursive(
        &self,
        dir: &Path,
        prefix: &str,
        tree: &mut String,
        depth: usize,
        emitted: &mut usize,
    ) {
        // 修复(S4,关键):walk_and_index 有 MAX_INDEX_ENTRIES=500 保护,但本函数此前只有
        // depth>4 的深度限制、**无总条目上限**。工作区若每层放大量子目录(4 层 × N 个),
        // build_directory_tree 会 read_dir 全部并 format! 全部名字拼进 String,内存与 CPU
        // 双重 DoS(用户执行 /index 即可触发)。加上 emitted 计数上限。
        const MAX_TREE_ENTRIES: usize = 2000;
        if depth > 4 || *emitted >= MAX_TREE_ENTRIES {
            if *emitted >= MAX_TREE_ENTRIES {
                tree.push_str(&format!(
                    "{}... (已达 {} 条目上限,省略剩余)\n",
                    prefix, MAX_TREE_ENTRIES
                ));
            } else {
                tree.push_str(&format!("{}...\n", prefix));
            }
            return;
        }

        if let Ok(entries) = std::fs::read_dir(dir) {
            let mut entries: Vec<_> = entries
                .flatten()
                .filter(|e| !should_skip_entry(&e.path()))
                .collect();
            entries.sort_by_key(|e| e.file_name());

            let total = entries.len();
            for (i, entry) in entries.iter().enumerate() {
                if *emitted >= MAX_TREE_ENTRIES {
                    break;
                }
                let is_last = i == total - 1;
                let connector = if is_last { "└── " } else { "├── " };
                let name = entry.file_name().to_string_lossy().to_string();

                if entry.path().is_dir() {
                    // 修复(Low-index):跳过符号链接目录,防 build_tree_recursive 环放大。
                    let is_symlink = entry
                        .path()
                        .symlink_metadata()
                        .map(|m| m.is_symlink())
                        .unwrap_or(false);
                    if is_symlink {
                        tree.push_str(&format!(
                            "{}{}{} -> (symlink, skipped)\n",
                            prefix, connector, name
                        ));
                        *emitted += 1;
                    } else {
                        tree.push_str(&format!("{}{}{}/\n", prefix, connector, name));
                        *emitted += 1;
                        let new_prefix = if is_last { "    " } else { "│   " };
                        self.build_tree_recursive(
                            &entry.path(),
                            &format!("{}{}", prefix, new_prefix),
                            tree,
                            depth + 1,
                            emitted,
                        );
                    }
                } else {
                    tree.push_str(&format!("{}{}{}\n", prefix, connector, name));
                    *emitted += 1;
                }
            }
        }
    }
}

/// 从文件内容中提取符号
fn extract_symbols(content: &str, language: &str) -> Vec<String> {
    let mut symbols = Vec::new();
    let max_symbols = 20;

    for line in content.lines().take(200) {
        if symbols.len() >= max_symbols {
            break;
        }

        let trimmed = line.trim();

        match language {
            "Rust" => {
                if let Some(name) = extract_rust_symbol(trimmed) {
                    symbols.push(name);
                }
            }
            "Python" => {
                if let Some(name) = extract_python_symbol(trimmed) {
                    symbols.push(name);
                }
            }
            "TypeScript" | "JavaScript" => {
                if let Some(name) = extract_js_symbol(trimmed) {
                    symbols.push(name);
                }
            }
            "Go" => {
                if let Some(name) = extract_go_symbol(trimmed) {
                    symbols.push(name);
                }
            }
            "Java" => {
                if let Some(name) = extract_java_symbol(trimmed) {
                    symbols.push(name);
                }
            }
            _ => {}
        }
    }

    symbols
}

fn extract_rust_symbol(line: &str) -> Option<String> {
    extract_symbol_by_prefixes(
        line,
        &[
            "pub fn ",
            "fn ",
            "pub async fn ",
            "async fn ",
            "pub struct ",
            "struct ",
            "pub enum ",
            "enum ",
            "pub trait ",
            "trait ",
            "pub type ",
            "type ",
            "impl ",
            "pub impl ",
        ],
        &['(', '{', '<'],
    )
}

fn extract_python_symbol(line: &str) -> Option<String> {
    extract_symbol_by_prefixes(line, &["def ", "async def ", "class "], &['('])
}

fn extract_js_symbol(line: &str) -> Option<String> {
    extract_symbol_by_prefixes(
        line,
        &[
            "function ",
            "export function ",
            "export default function ",
            "const ",
            "export const ",
            "class ",
            "export class ",
        ],
        &['(', '=', '{'],
    )
}

fn extract_go_symbol(line: &str) -> Option<String> {
    extract_symbol_by_prefixes(
        line,
        &["func ", "type ", "var ", "const ", "interface "],
        &['(', '{', ' '],
    )
}

fn extract_java_symbol(line: &str) -> Option<String> {
    extract_symbol_by_prefixes(
        line,
        &[
            "public class ",
            "private class ",
            "protected class ",
            "public interface ",
            "public enum ",
            "public void ",
            "public static ",
            "private void ",
        ],
        &['(', '{', ' '],
    )
}

/// 通用的符号提取函数：按前缀匹配后，用分割字符截取名称
fn extract_symbol_by_prefixes(
    line: &str,
    prefixes: &[&str],
    split_chars: &[char],
) -> Option<String> {
    for prefix in prefixes {
        if let Some(rest) = line.strip_prefix(prefix) {
            let name = rest
                .split(split_chars)
                .next()
                .unwrap_or(rest)
                .trim()
                .to_string();
            if !name.is_empty() && name.len() < 60 {
                return Some(format!("{}{}", prefix.trim_end(), name));
            }
        }
    }
    None
}

/// 提取导入列表
fn extract_import_list(content: &str, language: &str) -> Vec<String> {
    let mut imports = Vec::new();
    let max = 30;

    for line in content.lines() {
        if imports.len() >= max {
            break;
        }
        let trimmed = line.trim();

        match language {
            "Rust" => {
                if trimmed.starts_with("use ")
                    && let Some(path) = trimmed
                        .strip_prefix("use ")
                        .and_then(|s| s.split(';').next())
                        .map(|s| s.trim().to_string())
                {
                    imports.push(path);
                }
            }
            "Python" if (trimmed.starts_with("import ") || trimmed.starts_with("from ")) => {
                imports.push(trimmed.to_string());
            }
            "TypeScript" | "JavaScript" if trimmed.starts_with("import ") => {
                imports.push(trimmed.to_string());
            }
            "Go" if (trimmed.starts_with("import ") || trimmed.starts_with("\"")) => {
                imports.push(trimmed.to_string());
            }
            "Java" if trimmed.starts_with("import ") => {
                imports.push(trimmed.to_string());
            }
            _ => {}
        }
    }

    imports
}

/// 检测文件语言
fn detect_language(path: &Path) -> String {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "rs" => "Rust".into(),
        "py" => "Python".into(),
        "ts" | "tsx" => "TypeScript".into(),
        "js" | "jsx" => "JavaScript".into(),
        "go" => "Go".into(),
        "java" => "Java".into(),
        "c" | "h" => "C".into(),
        "cpp" | "hpp" | "cc" => "C++".into(),
        "rb" => "Ruby".into(),
        "php" => "PHP".into(),
        "swift" => "Swift".into(),
        "kt" => "Kotlin".into(),
        "scala" => "Scala".into(),
        "sh" | "bash" => "Shell".into(),
        "sql" => "SQL".into(),
        "html" => "HTML".into(),
        "css" | "scss" | "sass" | "less" => "CSS".into(),
        "json" => "JSON".into(),
        "yaml" | "yml" => "YAML".into(),
        "toml" => "TOML".into(),
        "md" => "Markdown".into(),
        _ => "Other".into(),
    }
}

/// 分类文件
fn categorize_file(path: &Path) -> FileCategory {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_lowercase();
    let parent = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_lowercase();

    if name.starts_with("test")
        || name.ends_with("_test")
        || name.ends_with(".test")
        || name.ends_with(".spec")
        || parent.contains("test")
        || parent.contains("__test__")
    {
        return FileCategory::Test;
    }

    if name.ends_with(".md")
        || name.ends_with(".rst")
        || name.ends_with(".txt")
        || parent.contains("doc")
    {
        return FileCategory::Documentation;
    }

    if name.ends_with(".toml")
        || name.ends_with(".yaml")
        || name.ends_with(".yml")
        || name.ends_with(".json")
        || name == "makefile"
        || name == "dockerfile"
        || name.ends_with(".ini")
        || name.ends_with(".cfg")
        || name.ends_with(".conf")
        || name == ".env"
        || name == ".gitignore"
    {
        return FileCategory::Config;
    }

    if name.ends_with(".png")
        || name.ends_with(".jpg")
        || name.ends_with(".svg")
        || name.ends_with(".ico")
        || name.ends_with(".woff")
        || name.ends_with(".ttf")
    {
        return FileCategory::Asset;
    }

    if parent.contains("target")
        || parent.contains("dist")
        || parent.contains("build")
        || parent.contains("node_modules")
        || parent.contains(".output")
    {
        return FileCategory::Build;
    }

    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if [
        "rs", "py", "ts", "tsx", "js", "jsx", "go", "java", "c", "cpp", "rb", "php", "swift", "kt",
        "scala",
    ]
    .contains(&ext)
    {
        return FileCategory::Source;
    }

    FileCategory::Other
}

/// 判断是否应跳过
fn should_skip_entry(path: &Path) -> bool {
    let skip = [
        ".git",
        "node_modules",
        "target",
        "__pycache__",
        ".venv",
        "venv",
        "dist",
        "build",
        ".next",
        ".nuxt",
        "coverage",
        ".cache",
        ".movix",
        ".idea",
        ".vscode",
        "vendor",
        "Pods",
        ".gradle",
        ".mvn",
    ];

    if path.is_dir()
        && let Some(name) = path.file_name().and_then(|n| n.to_str())
    {
        return skip.contains(&name);
    }
    false
}

/// 判断是否可索引
fn is_indexable_file(path: &Path) -> bool {
    let exts = [
        "rs", "py", "ts", "tsx", "js", "jsx", "go", "java", "c", "cpp", "h", "hpp", "rb", "php",
        "swift", "kt", "scala", "sh", "toml", "yaml", "yml", "json", "md", "sql", "html", "css",
        "scss",
    ];
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| exts.contains(&e))
        .unwrap_or(false)
}
