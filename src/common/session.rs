use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::common::deepseek::ChatMessage;

/// 历史会话最大保留数。修复：原实现每次 save 都额外写一份时间戳文件,
/// 长期使用会膨胀到几千个 JSON。通过环境变量 `MOVIX_MAX_SESSION_HISTORY` 调整。
const DEFAULT_MAX_SESSION_HISTORY: usize = 20;

/// 会话状态快照，可序列化到磁盘
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    /// 会话创建时间
    pub created_at: String,
    /// 最后更新时间
    pub updated_at: String,
    /// 对话消息历史
    pub messages: Vec<ChatMessage>,
    /// 当前运行模式（u8 编码）
    pub mode: u8,
    /// 会话 token 统计
    pub total_tokens: u64,
    /// 会话轮次
    pub turns: u32,
    /// 工作目录
    pub workspace: String,
}

/// 会话持久化管理器
#[derive(Clone)]
pub struct SessionPersistence {
    session_dir: PathBuf,
    workspace: PathBuf,
}

impl SessionPersistence {
    /// 创建会话持久化管理器
    pub fn new(workspace: &Path) -> Self {
        // 修复(M2,关键):原实现手写 env var 查找 + 静默降级到 `.`。utils::home_dir() 已
        // 用 `dirs` crate 并明确点名此处的 bug(容器/无 HOME 下 session 写进 cwd)。
        // 此前修复 utils.rs 时漏改本处,导致 bug 依旧。现在统一走 utils::home_dir。
        let home = crate::common::utils::home_dir();
        let workspace_hash = format!("{:x}", simple_hash(workspace.to_string_lossy().as_bytes()));
        let session_dir = home.join(".movix").join("sessions").join(&workspace_hash);

        Self {
            session_dir,
            workspace: workspace.to_path_buf(),
        }
    }

    /// 获取最新会话文件路径
    pub fn latest_session_path(&self) -> PathBuf {
        self.session_dir.join("latest.json")
    }

    /// 保存会话快照到磁盘
    pub fn save(&self, snapshot: &SessionSnapshot) -> std::io::Result<()> {
        fs::create_dir_all(&self.session_dir)?;

        let json = serde_json::to_string(snapshot)?;
        let latest_path = self.latest_session_path();

        // 修复：原实现在 remove_file 和 rename 之间崩溃会导致 latest.json 丢失。
        // 改为先写 tmp，再直接 rename 覆盖（Unix 上 rename 是原子操作，
        // Windows 上若目标已存在则 rename 会失败，此时先 remove 再 rename，
        // 但顺序为 tmp→backup→rename，确保至少有一个完整文件存在）。
        let tmp_path = latest_path.with_extension("tmp");
        // 修复(审查):会话文件含 agent 看过的文件内容,用 0600 权限写入,防止同机
        // 其他用户读取明文(此前默认 umask 0644)。
        crate::common::utils::write_private(&tmp_path, &json)?;

        // 先尝试直接 rename（原子覆盖，Linux/Mac 支持）
        if fs::rename(&tmp_path, &latest_path).is_err() {
            // Windows 或目标已存在：先删除旧文件再 rename
            // 此时有短暂窗口 latest.json 不存在，但 tmp_path 始终有效
            if latest_path.exists() {
                let backup_path = latest_path.with_extension("bak");
                let _ = fs::rename(&latest_path, &backup_path);
                // 修复: fallback 路径生成的 .bak 文件后续不会进 prune_history,
                // 会导致无限累积。在成功 write 后清理。
                let _ = fs::remove_file(&backup_path);
            }
            fs::rename(&tmp_path, &latest_path)?;
        }

        // 修复:原实现用 `%Y%m%d_%H%M%S` 时间戳,同秒内多次保存会用同一文件名
        // 互相覆盖,丢失历史快照。改为在秒级时间戳基础上拼一个递增序号,
        // 同一秒内第二次起序号自增,避免覆盖。
        let base_ts = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
        let mut history_path = self.session_dir.join(format!("session_{}.json", base_ts));
        let mut suffix: u32 = 0;
        while history_path.exists() {
            history_path = self
                .session_dir
                .join(format!("session_{}_{}.json", base_ts, suffix));
            suffix = suffix.saturating_add(1);
            // 防御性兜底:如果序号都跑完了(几乎不可能),退回临时文件避免无限循环
            if suffix > 10_000 {
                history_path = self.session_dir.join(format!(
                    "session_{}_{}.json.tmp",
                    base_ts,
                    std::process::id()
                ));
                break;
            }
        }
        let _ = crate::common::utils::write_private(&history_path, &json);

        // 清理多余的历史会话,只保留最近 N 个 + latest。
        let max_keep = std::env::var("MOVIX_MAX_SESSION_HISTORY")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(DEFAULT_MAX_SESSION_HISTORY)
            .max(1);
        self.prune_history(max_keep);

        Ok(())
    }

    /// 保留最近 N 个历史快照(按文件名时间戳排序,旧的删除)。
    fn prune_history(&self, keep: usize) {
        let Ok(entries) = fs::read_dir(&self.session_dir) else {
            return;
        };
        let mut history_files: Vec<(PathBuf, String)> = entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with("session_") && name.ends_with(".json") {
                    Some((e.path(), name))
                } else {
                    None
                }
            })
            .collect();
        // 修复(M3):兜底路径(suffix>10000)生成的 `session_*.json.tmp` 后缀不匹配 `.json`,
        // prune 永远删不掉它,造成持久残留。这里顺手清理这类孤儿 .tmp 文件。
        if let Ok(tmp_entries) = fs::read_dir(&self.session_dir) {
            for e in tmp_entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with("session_") && name.ends_with(".json.tmp") {
                    let _ = fs::remove_file(e.path());
                }
            }
        }
        // 文件名按时间戳逆序,最新在前
        history_files.sort_by(|a, b| b.1.cmp(&a.1));
        for (path, _) in history_files.into_iter().skip(keep) {
            if let Err(e) = fs::remove_file(&path) {
                tracing::warn!(target: "session", "清理历史会话失败 {:?}: {}", path, e);
            }
        }
    }

    /// 加载最新的会话快照
    pub fn load_latest(&self) -> Option<SessionSnapshot> {
        let path = self.latest_session_path();
        if !path.exists() {
            return None;
        }
        let content = fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// 检查是否存在可恢复的会话
    pub fn has_session(&self) -> bool {
        self.latest_session_path().exists()
    }

    /// 删除会话快照
    pub fn clear(&self) -> std::io::Result<()> {
        let path = self.latest_session_path();
        if path.exists() {
            fs::remove_file(&path)?;
        }
        Ok(())
    }

    /// 列出所有历史会话
    pub fn list_sessions(&self) -> Vec<(String, PathBuf)> {
        if !self.session_dir.exists() {
            return Vec::new();
        }
        let mut sessions = Vec::new();
        if let Ok(entries) = fs::read_dir(&self.session_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "json").unwrap_or(false) {
                    let name = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("unknown")
                        .to_string();
                    // 修复(Low):过滤掉 latest.json —— 它是当前会话的指针文件,
                    // 列入可恢复列表会造成混淆(恢复它等于恢复当前状态,无操作)。
                    if name == "latest" {
                        continue;
                    }
                    let modified = entry
                        .metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                    sessions.push((name, path, modified));
                }
            }
        }
        sessions.sort_by_key(|s| std::cmp::Reverse(s.2));
        sessions
            .into_iter()
            .map(|(name, path, _)| (name, path))
            .collect()
    }

    /// 从当前 Agent 状态创建快照。
    /// 修复：原实现每次都用当前时间覆盖 `created_at`，恢复会话后再次保存时
    /// 原始创建时间丢失。现在优先从已有会话中保留原始 `created_at`。
    pub fn create_snapshot(
        &self,
        messages: &[ChatMessage],
        mode: u8,
        total_tokens: u64,
        turns: u32,
    ) -> SessionSnapshot {
        let now = chrono::Utc::now().to_rfc3339();
        let created_at = self
            .load_latest()
            .map(|s| s.created_at)
            .unwrap_or_else(|| now.clone());
        SessionSnapshot {
            created_at,
            updated_at: now,
            messages: messages.to_vec(),
            mode,
            total_tokens,
            turns,
            workspace: self.workspace.to_string_lossy().to_string(),
        }
    }
}

fn simple_hash(data: &[u8]) -> u64 {
    // 修复(M1,关键):原用 `DefaultHasher`,Rust 标准库未承诺其算法跨版本稳定。snapshot.rs
    // 的同名函数已特意改成 FNV-1a 并注明原因,但本处漏改 → 工具链升级后同一 workspace 的
    // session 目录哈希变化,旧 `~/.movix/sessions/<hash>/` 全部孤儿化,会话历史"丢失"。
    // 现在对齐 snapshot.rs,统一用 FNV-1a(算法固定、无外部依赖)。
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut hash = FNV_OFFSET;
    for &b in data {
        hash ^= b as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn snapshot(turns: u32) -> SessionSnapshot {
        SessionSnapshot {
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            messages: Vec::new(),
            mode: 0,
            total_tokens: 0,
            turns,
            workspace: "test".to_string(),
        }
    }

    #[test]
    fn save_replaces_existing_latest() {
        let dir = TempDir::new().unwrap();
        let persistence = SessionPersistence {
            session_dir: dir.path().join("sessions"),
            workspace: dir.path().to_path_buf(),
        };

        persistence.save(&snapshot(1)).unwrap();
        persistence.save(&snapshot(2)).unwrap();

        let loaded = persistence.load_latest().unwrap();
        assert_eq!(loaded.turns, 2);
    }
}
