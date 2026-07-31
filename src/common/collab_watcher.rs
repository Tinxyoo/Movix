use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// 把 SystemTime mtime 转成纳秒(u128,自 UNIX_EPOCH)。
/// 修复(M3):用亚秒精度,避免秒级 mtime 把同一秒内的并发编辑判为"未变化"。
fn mtime_nanos(meta: &std::fs::Metadata) -> Option<u128> {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
}

/// 文件变更类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeType {
    /// 文件被创建
    Created,
    /// 文件被修改
    Modified,
    /// 文件被删除
    Deleted,
}

/// 文件变更记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    /// 文件路径
    pub path: PathBuf,
    /// 变更类型
    pub change_type: ChangeType,
    /// 检测到变更的时间
    pub detected_at: SystemTime,
    /// 变更前的修改时间（纳秒,self epoch;如果有）
    pub previous_mtime: Option<u128>,
    /// 变更后的修改时间(纳秒, self epoch)
    pub current_mtime: Option<u128>,
}

/// 协作编辑感知器
pub struct CollaborationWatcher {
    workspace: PathBuf,
    /// 文件修改时间快照(纳秒自 UNIX_EPOCH)。
    /// 修复(M3):原用秒级 mtime,同一秒内的两次编辑(人工 + agent)产生相同 mtime
    /// → detect_changes 报"无变化" → agent 可能覆盖用户刚做的并发修改。改用纳秒精度。
    mtime_snapshot: HashMap<PathBuf, u128>,
    /// 上次检查时间
    last_check: Option<SystemTime>,
    /// 忽略的路径模式
    ignore_patterns: Vec<String>,
    /// 是否启用
    enabled: bool,
}

impl CollaborationWatcher {
    /// 创建协作编辑感知器
    pub fn new(workspace: &Path) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
            mtime_snapshot: HashMap::new(),
            last_check: None,
            ignore_patterns: vec![
                ".git".into(),
                "target".into(),
                "node_modules".into(),
                "__pycache__".into(),
                ".movix".into(),
                "dist".into(),
                "build".into(),
            ],
            enabled: true,
        }
    }

    /// 设置是否启用
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// 初始化快照（记录当前所有文件的修改时间）
    pub fn initialize(&mut self) {
        // 修复: clear+take 对同一 HashMap 做两次清空,且 clone workspace 不必要。
        self.mtime_snapshot.clear();
        Self::scan_to_depth_static(
            &self.workspace,
            &mut self.mtime_snapshot,
            0,
            &self.ignore_patterns,
        );
        self.last_check = Some(SystemTime::now());
    }

    /// 检测自上次检查以来的文件变更
    pub fn detect_changes(&mut self) -> Vec<FileChange> {
        if !self.enabled {
            return Vec::new();
        }

        let mut changes = Vec::new();
        let mut current_files: HashMap<PathBuf, u128> = HashMap::new();

        self.scan_to_map(&self.workspace, &mut current_files);

        for (path, current_mtime) in &current_files {
            match self.mtime_snapshot.get(path) {
                Some(prev_mtime) => {
                    if current_mtime != prev_mtime {
                        changes.push(FileChange {
                            path: path.clone(),
                            change_type: ChangeType::Modified,
                            detected_at: SystemTime::now(),
                            previous_mtime: Some(*prev_mtime),
                            current_mtime: Some(*current_mtime),
                        });
                    }
                }
                None => {
                    changes.push(FileChange {
                        path: path.clone(),
                        change_type: ChangeType::Created,
                        detected_at: SystemTime::now(),
                        previous_mtime: None,
                        current_mtime: Some(*current_mtime),
                    });
                }
            }
        }

        for (path, prev_mtime) in &self.mtime_snapshot {
            if !current_files.contains_key(path) {
                changes.push(FileChange {
                    path: path.clone(),
                    change_type: ChangeType::Deleted,
                    detected_at: SystemTime::now(),
                    previous_mtime: Some(*prev_mtime),
                    current_mtime: None,
                });
            }
        }

        self.mtime_snapshot = current_files;
        self.last_check = Some(SystemTime::now());

        changes
    }

    /// 检查指定文件是否被外部修改
    pub fn is_file_changed(&self, path: &Path) -> bool {
        if !self.enabled {
            return false;
        }

        match self.mtime_snapshot.get(path) {
            Some(snapshot_mtime) => match std::fs::metadata(path) {
                Ok(meta) => {
                    let current_mtime = mtime_nanos(&meta);
                    current_mtime != Some(*snapshot_mtime)
                }
                Err(_) => true,
            },
            None => path.exists(),
        }
    }

    /// 读取变更文件的内容差异描述
    pub fn describe_changes(&self, changes: &[FileChange]) -> String {
        if changes.is_empty() {
            return "No external changes detected.".into();
        }

        let mut parts = Vec::new();
        parts.push(format!("<external_changes count=\"{}\">", changes.len()));

        for change in changes {
            let rel_path = change
                .path
                .strip_prefix(&self.workspace)
                .unwrap_or(&change.path)
                .display();

            match change.change_type {
                ChangeType::Created => {
                    parts.push(format!("  + {} (new file)", rel_path));
                }
                ChangeType::Modified => {
                    parts.push(format!("  ~ {} (modified externally)", rel_path));
                }
                ChangeType::Deleted => {
                    parts.push(format!("  - {} (deleted externally)", rel_path));
                }
            }
        }

        parts.push("</external_changes>".into());
        parts.join("\n")
    }

    /// 标记文件已被 Agent 修改（避免误报为外部修改）
    pub fn mark_agent_modified(&mut self, path: &Path) {
        if let Some(m) = std::fs::metadata(path).ok().as_ref().and_then(mtime_nanos) {
            self.mtime_snapshot.insert(path.to_path_buf(), m);
        }
    }

    /// 递归扫描到 HashMap（独立函数，不依赖 &self 避免借用冲突）
    fn scan_to_depth_static(
        dir: &Path,
        map: &mut HashMap<PathBuf, u128>,
        depth: usize,
        ignore_patterns: &[String],
    ) {
        const MAX_SCAN_DEPTH: usize = 20;
        // 修复(M-collab):增加条目上限,防止大/恶意工作区下扫描无界增长 + 阻塞 runtime。
        const MAX_SCAN_ENTRIES: usize = 50_000;
        if depth > MAX_SCAN_DEPTH || map.len() > MAX_SCAN_ENTRIES {
            return;
        }
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if map.len() > MAX_SCAN_ENTRIES {
                    return;
                }
                let path = entry.path();
                if Self::should_ignore_static(&path, ignore_patterns) {
                    continue;
                }
                // 修复(M-collab,关键):原用 `path.is_dir()` / `entry.metadata()`,二者**跟随
                // 符号链接**。若工作区存在符号链接环(指向祖先目录),is_dir 返回 true 会反复
                // 进入同一物理目录,虽 depth 递增最终会停,但每层都重复 read_dir + insert,
                // 造成大量重复 I/O 与 CPU 浪费。改用 symlink_metadata(不跟随),对符号链接
                // 一律当文件处理(记录但不递归),彻底杜绝环。
                let meta = match entry.path().symlink_metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if meta.is_symlink() {
                    // 符号链接:记录其自身 mtime,不递归(防环)。
                    if let Some(m) = mtime_nanos(&meta) {
                        map.insert(path, m);
                    }
                } else if meta.is_dir() {
                    Self::scan_to_depth_static(&path, map, depth + 1, ignore_patterns);
                } else {
                    if let Some(m) = mtime_nanos(&meta) {
                        map.insert(path, m);
                    }
                }
            }
        }
    }

    fn should_ignore_static(path: &Path, ignore_patterns: &[String]) -> bool {
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if ignore_patterns.iter().any(|p| name == p.as_str()) {
                return true;
            }
            if name.starts_with('.') {
                let allowed_dot_dirs = [".github", ".vscode", ".idea", ".config"];
                return !allowed_dot_dirs.contains(&name);
            }
        }
        false
    }

    /// 递归扫描到 HashMap（实例方法，用于外部调用方便）
    fn scan_to_map(&self, dir: &Path, map: &mut HashMap<PathBuf, u128>) {
        Self::scan_to_depth_static(dir, map, 0, &self.ignore_patterns);
    }
}
