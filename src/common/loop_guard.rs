use std::collections::VecDeque;
use std::hash::{Hash, Hasher};

use std::collections::hash_map::DefaultHasher;

use crate::common::deepseek::ToolCall;

const IDENTICAL_CALL_BLOCK_THRESHOLD: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptDecision {
    Proceed,
    Block(String),
}

#[derive(Debug, Clone)]
struct RecentEntry {
    name: String,
    args_hash: u64,
    read_only: bool,
}

pub type IsMutating = fn(&ToolCall) -> bool;

/// 工具调用风暴守卫:检测同一工具以相同参数被反复调用的循环。
///
/// 维护一个固定大小的"最近调用"滑动窗口,若窗口内同一 (工具名, 参数哈希)
/// 组合出现次数达到阈值,则拦截。mutating 工具会先清除窗口内所有 read_only
/// 条目,避免只读调用的噪声淹没真正的写入循环。
#[derive(Debug)]
pub struct LoopGuard {
    storm_window_size: usize,
    storm_threshold: u32,
    is_mutating: Option<IsMutating>,
    recent: VecDeque<RecentEntry>,
}

impl LoopGuard {
    pub fn new() -> Self {
        Self {
            storm_window_size: 6,
            storm_threshold: IDENTICAL_CALL_BLOCK_THRESHOLD,
            is_mutating: None,
            recent: VecDeque::new(),
        }
    }

    pub fn set_mutating_checker(&mut self, checker: IsMutating) {
        self.is_mutating = Some(checker);
    }

    pub fn inspect_tool_call(&mut self, call: &ToolCall) -> AttemptDecision {
        self.inspect_raw(&call.function.name, &call.function.arguments)
    }

    /// 核心检查逻辑，直接接收 name 和序列化后的 args_str
    fn inspect_raw(&mut self, name: &str, args_str: &str) -> AttemptDecision {
        let mutating = self.is_mutating.is_some_and(|check| {
            // 临时构造 ToolCall 供 checker 使用，仅在 checker 存在时
            let call = ToolCall {
                id: String::new(),
                call_type: "function".into(),
                function: crate::common::deepseek::FunctionCall {
                    name: name.to_string(),
                    arguments: args_str.to_string(),
                },
            };
            check(&call)
        });
        let read_only = !mutating;

        if mutating {
            self.recent.retain(|entry| !entry.read_only);
        }

        // 修复：对 JSON 中的路径字段做规范化，防止语义等价但字面不同的路径
        // 绕过重复检测（如 "src/main.rs" vs "./src/main.rs" vs "src/./main.rs"）
        let normalized_args = normalize_paths_in_json(args_str);
        let args_hash = hash_json_str(&normalized_args);
        let count = self
            .recent
            .iter()
            .filter(|e| e.name == name && e.args_hash == args_hash)
            .count() as u32;

        if count >= self.storm_threshold.saturating_sub(1) {
            return AttemptDecision::Block(format!(
                "`{}` called with identical args {} times — repeat-loop guard tripped",
                name,
                count + 1
            ));
        }

        self.recent.push_back(RecentEntry {
            name: name.to_string(),
            args_hash,
            read_only,
        });

        while self.recent.len() > self.storm_window_size {
            self.recent.pop_front();
        }

        AttemptDecision::Proceed
    }

    pub fn reset_storm(&mut self) {
        self.recent.clear();
    }
}

impl Default for LoopGuard {
    fn default() -> Self {
        Self::new()
    }
}

fn hash_json_str(s: &str) -> u64 {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(s) {
        // serde_json 的 Map 默认按 key 排序，to_string 输出即为规范化形式
        let canonical = serde_json::to_string(&v).unwrap_or_default();
        let mut hasher = DefaultHasher::new();
        canonical.hash(&mut hasher);
        hasher.finish()
    } else {
        let mut hasher = DefaultHasher::new();
        s.hash(&mut hasher);
        hasher.finish()
    }
}

/// 对 JSON 参数中的路径字段做规范化：去除 "./" 前缀和 "/./" 段，
/// 使 "src/main.rs"、"./src/main.rs"、"src/./main.rs" 产生相同的哈希。
/// 只处理常见的路径字段名，其他字段保持不变。
fn normalize_paths_in_json(args_str: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(args_str) else {
        return args_str.to_string();
    };

    let path_keys = ["path", "file_path", "cwd", "dir", "url"];
    if let Some(obj) = value.as_object_mut() {
        for key in &path_keys {
            if let Some(path_val) = obj.get(*key).and_then(|v| v.as_str()) {
                let normalized = normalize_path_str(path_val);
                obj.insert(key.to_string(), serde_json::Value::String(normalized));
            }
        }
    }

    serde_json::to_string(&value).unwrap_or_else(|_| args_str.to_string())
}

/// 规范化单个路径字符串：去除 "./" 前缀和 "/./" 段。
fn normalize_path_str(path: &str) -> String {
    let mut result = path.to_string();
    // 反复去除 "./" 前缀
    while result.starts_with("./") {
        result = result[2..].to_string();
    }
    // 反复去除 "/./" 中间段
    while result.contains("/./") {
        result = result.replace("/./", "/");
    }
    // 去除末尾的 "/."
    if result.ends_with("/.") {
        result.truncate(result.len() - 2);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_identical_call_block() {
        let mut guard = LoopGuard::new();
        let call = ToolCall {
            id: "1".into(),
            call_type: "function".into(),
            function: crate::common::deepseek::FunctionCall {
                name: "read_file".into(),
                arguments: r#"{"path":"/foo"}"#.into(),
            },
        };

        assert_eq!(guard.inspect_tool_call(&call), AttemptDecision::Proceed);
        assert_eq!(guard.inspect_tool_call(&call), AttemptDecision::Proceed);
        assert!(matches!(
            guard.inspect_tool_call(&call),
            AttemptDecision::Block(_)
        ));
    }

    #[test]
    fn test_different_args_proceed() {
        let mut guard = LoopGuard::new();
        let c1 = ToolCall {
            id: "1".into(),
            call_type: "function".into(),
            function: crate::common::deepseek::FunctionCall {
                name: "read_file".into(),
                arguments: r#"{"path":"/foo"}"#.into(),
            },
        };
        let c2 = ToolCall {
            id: "2".into(),
            call_type: "function".into(),
            function: crate::common::deepseek::FunctionCall {
                name: "read_file".into(),
                arguments: r#"{"path":"/bar"}"#.into(),
            },
        };

        assert_eq!(guard.inspect_tool_call(&c1), AttemptDecision::Proceed);
        assert_eq!(guard.inspect_tool_call(&c2), AttemptDecision::Proceed);
    }

    #[test]
    fn test_path_normalization_equivalence() {
        let mut guard = LoopGuard::new();
        let c1 = ToolCall {
            id: "1".into(),
            call_type: "function".into(),
            function: crate::common::deepseek::FunctionCall {
                name: "read_file".into(),
                arguments: r#"{"path":"src/main.rs"}"#.into(),
            },
        };
        let c2 = ToolCall {
            id: "2".into(),
            call_type: "function".into(),
            function: crate::common::deepseek::FunctionCall {
                name: "read_file".into(),
                arguments: r#"{"path":"./src/main.rs"}"#.into(),
            },
        };
        let c3 = ToolCall {
            id: "3".into(),
            call_type: "function".into(),
            function: crate::common::deepseek::FunctionCall {
                name: "read_file".into(),
                arguments: r#"{"path":"src/./main.rs"}"#.into(),
            },
        };
        // 三种等价路径写法应被视为同一调用,触发风暴守卫
        assert_eq!(guard.inspect_tool_call(&c1), AttemptDecision::Proceed);
        assert_eq!(guard.inspect_tool_call(&c2), AttemptDecision::Proceed);
        assert!(matches!(
            guard.inspect_tool_call(&c3),
            AttemptDecision::Block(_)
        ));
    }
}
