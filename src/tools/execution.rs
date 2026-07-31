use std::sync::Arc;

use serde_json::Value;

use crate::common::arg_repair;

pub type ToolCallback = Arc<dyn Fn(String, String, Option<String>, String) + Send + Sync>;

#[derive(Debug, Clone)]
pub struct ToolCallDetail {
    pub detail: String,
    pub file: Option<String>,
    pub args: Value,
    /// 修复(G-H7):标记参数是否过大。原实现把 args 设为 Null 但不暴露标志,
    /// 导致 execute_tool_with_repair 见 pre_parsed=Some 就合成 too_large:false 的
    /// report,安全闸门永不触发。现在显式透传该标志。
    pub too_large: bool,
    /// 修复(C3,关键):**未经截断**的完整命令文本,专供风险判定使用。
    /// `detail` 字段会被截断到 40 字符用于 UI 展示,若风险判定也用 `detail`,
    /// 攻击者可用 `printf 'xxx...'; rm -rf /opt` 把 `rm ` 挤出 40 字符窗口,
    /// 绕过 Auto 模式高风险审批。此字段始终保留完整命令,仅用于安全判定。
    pub full_command: Option<String>,
}

pub fn extract_tool_detail(tool_name: &str, arguments: &str) -> ToolCallDetail {
    let (args, report) = arg_repair::repair_with_report(arguments);

    // 安全检查：如果参数过大（超过 MAX_ARG_LEN），返回空 detail 并标记 args 为 Null。
    if report.too_large {
        return ToolCallDetail {
            detail: format!("[参数过大: {} bytes]", report.original_len),
            file: None,
            args: serde_json::Value::Null,
            too_large: true,
            full_command: None,
        };
    }

    // 提取完整命令(C3):用于风险判定,绝不截断。
    let full_command = if tool_name == "run_command" {
        args.get("command")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    } else {
        None
    };

    let detail = match tool_name {
        "read_file" | "write_file" | "patch_file" => args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "list_dir" => args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "run_command" => {
            let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            crate::common::utils::truncate_str(cmd, 40)
        }
        "search_code" | "grep" => args
            .get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "web_search" => args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "web_fetch" => args
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        _ => tool_name.to_string(),
    };

    let file = if tool_name == "write_file" || tool_name == "patch_file" {
        args.get("path")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    } else {
        None
    };

    ToolCallDetail {
        detail,
        file,
        args,
        too_large: false,
        full_command,
    }
}

pub fn extract_file_from_args(arguments: &str) -> Option<String> {
    let (args, _) = arg_repair::repair_with_report(arguments);
    args.get("path")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_file_for_write_and_patch_tools() {
        let write = extract_tool_detail("write_file", r#"{"path":"src/main.rs"}"#);
        assert_eq!(write.file.as_deref(), Some("src/main.rs"));

        let patch = extract_tool_detail("patch_file", r#"{"path":"src/lib.rs","patches":[]}"#);
        assert_eq!(patch.file.as_deref(), Some("src/lib.rs"));
    }

    #[test]
    fn extracts_search_detail_from_pattern() {
        let detail = extract_tool_detail("grep", r#"{"pattern":"ToolCallback"}"#);
        assert_eq!(detail.detail, "ToolCallback");
    }

    #[test]
    fn truncates_long_command_detail() {
        let detail = extract_tool_detail(
            "run_command",
            r#"{"command":"echo 123456789012345678901234567890123456789012345"}"#,
        );
        assert!(detail.detail.ends_with('…'));
        assert!(detail.detail.chars().count() <= 41);
    }

    /// 回归(G-H7):参数过大时 ToolCallDetail 必须设置 too_large=true,
    /// 让 handle_tool_call 据此拒绝执行(否则 execute_tool_with_repair 见
    /// pre_parsed=Some 就合成 too_large:false 的 report,闸门失效)。
    #[test]
    fn too_large_flag_set_on_oversized_args() {
        // 构造一个超过 MAX_ARG_LEN 的参数。
        let huge = "x".repeat(crate::common::arg_repair::MAX_ARG_LEN + 100);
        let args = format!(r#"{{"content":"{}"}}"#, huge);
        let detail = extract_tool_detail("write_file", &args);
        assert!(
            detail.too_large,
            "参数超过 MAX_ARG_LEN 时 too_large 必须为 true"
        );
    }

    /// 回归(G-H7):正常大小参数 too_large 必须为 false。
    #[test]
    fn too_large_flag_false_on_normal_args() {
        let detail = extract_tool_detail("write_file", r#"{"path":"a.rs","content":"hi"}"#);
        assert!(!detail.too_large);
    }
}
