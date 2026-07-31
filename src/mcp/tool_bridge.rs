use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::Mutex;

use serde_json::Value;

use crate::common::deepseek::ToolDefinition;
use crate::common::error::Result;

use crate::mcp::client::{McpManager, McpToolInfo};
use crate::tools::{EffectKind, RiskLevel, Tool, ToolResult};

/// MCP 工具桥接器，将 MCP 服务器上的工具适配为本地 Tool trait 实现
pub struct McpToolBridge {
    server_name: String,
    prefixed_name: String,
    tool_info: McpToolInfo,
    manager: Arc<Mutex<McpManager>>,
}

impl McpToolBridge {
    /// 创建 MCP 工具桥接器
    pub fn new(
        server_name: String,
        tool_info: McpToolInfo,
        manager: Arc<Mutex<McpManager>>,
    ) -> Self {
        let prefixed_name = Self::prefixed_name(&server_name, &tool_info.name);
        Self {
            server_name,
            prefixed_name,
            tool_info,
            manager,
        }
    }

    /// 生成带前缀的工具名，避免不同服务器的工具名冲突
    /// 格式: mcp__{server}__{tool}
    ///
    /// 修复(Medium #M8):若 server_name 含 `__`,反解析会出错。这里在构造时
    /// 记录 warning,提示命名不规范(但不阻断——MCP 名字未做字符约束)。
    pub fn prefixed_name(server_name: &str, tool_name: &str) -> String {
        if server_name.contains("__") {
            tracing::warn!(
                target: "mcp",
                "MCP 服务器名 '{}' 含 '__',将导致 parse_prefixed_name 反解析错误",
                server_name
            );
        }
        format!("mcp__{}__{}", server_name, tool_name)
    }

    /// 从带前缀的工具名解析出服务器名和原始工具名。
    ///
    /// 格式 `mcp__{server}__{tool}` 在 server 名含 `__` 时有歧义。我们约定
    /// **第一个** `__` 之后的全部内容属于 tool(server 名不得含 `__`,
    /// `prefixed_name` 构造时会 warn)。splitn(2) 保证只在第一个 `__` 处切一次,
    /// 这样 tool 名(允许含 `__`)能完整保留。
    pub fn parse_prefixed_name(prefixed: &str) -> Option<(String, String)> {
        let rest = prefixed.strip_prefix("mcp__")?;
        // splitn(2, "__"):最多切 1 刀,第一个 `__` 后的全部归 tool。
        let mut parts = rest.splitn(2, "__");
        let server = parts.next()?.to_string();
        let tool = parts.next()?.to_string();
        if server.is_empty() || tool.is_empty() {
            return None;
        }
        Some((server, tool))
    }

    /// 修复(R5/H10):校验 arguments 是否包含 input_schema 中声明的所有 required 字段。
    /// 返回 Some(缺失字段描述) 表示校验失败,None 表示通过(或无 required 声明)。
    /// 这是 JSON Schema 校验的最有价值子集,挡住"缺必要参数 → 工具用危险默认值"。
    fn check_required_fields(&self, arguments: &Value) -> Option<String> {
        let schema = self.tool_info.input_schema.as_ref()?;
        // 仅对 type=object 的 schema 校验 required。
        let required = schema.get("required")?.as_array()?;
        if required.is_empty() {
            return None;
        }
        let obj = match arguments.as_object() {
            Some(o) => o,
            None => {
                // schema 要求 object 但参数不是 object → 校验失败。
                return Some("(参数非 object)".to_string());
            }
        };
        let missing: Vec<&str> = required
            .iter()
            .filter_map(|r| r.as_str())
            .filter(|name| !obj.contains_key(*name))
            .collect();
        if missing.is_empty() {
            None
        } else {
            Some(missing.join(", "))
        }
    }
}

#[async_trait]
impl Tool for McpToolBridge {
    fn name(&self) -> &str {
        &self.prefixed_name
    }

    fn description(&self) -> &str {
        self.tool_info.description.as_deref().unwrap_or("MCP tool")
    }

    fn parameters(&self) -> Value {
        self.tool_info.input_schema.clone().unwrap_or_else(|| {
            serde_json::json!({
                "type": "object",
                "properties": {}
            })
        })
    }

    async fn execute(&self, arguments: &Value) -> Result<ToolResult> {
        // 修复(P2.2):此前 `let manager = self.manager.lock().await` 之后再
        // `manager.call_tool().await` —— 整个 MCP 工具调用期间(可能 30s 网络等待)
        // 都持有 manager 锁,其它 MCP 工具(哪怕来自完全不同的 server)全部排队。
        // 现在改为短锁:在 manager 锁内只取出 client 句柄(Arc 克隆,瞬时),
        // 真正 await call_tool 时只持有该 server 自己的 client 锁。
        let handle = {
            let manager = self.manager.lock().await;
            manager.client_handle(&self.server_name)
        };
        let Some(handle) = handle else {
            return Ok(ToolResult::err(format!(
                "MCP 服务器 '{}' 不存在",
                self.server_name
            )));
        };

        // 修复(R5/H10,关键):MCP 工具的 input_schema 此前从不校验,LLM/arg_repair
        // 产生的参数(可能违反 required/约束)原样转发外部进程。完整 JSON Schema 校验
        // 需引入新依赖;这里做最有价值的子集校验:required 字段必须存在。
        // 这挡住最危险的"缺必要参数 → 工具用危险默认值"场景。
        // (在加锁/调用前校验,避免不必要的远程调用。)
        if let Some(missing) = self.check_required_fields(arguments) {
            return Ok(ToolResult::err(format!(
                "MCP 工具 '{}' 参数校验失败:缺少必要字段 {}。已拒绝执行以避免工具用危险默认值。",
                self.tool_info.name, missing
            )));
        }

        let call_result = {
            let mut client = handle.lock().await;
            if !matches!(
                client.status(),
                crate::mcp::client::McpServerStatus::Connected
            ) {
                return Ok(ToolResult::err(format!(
                    "MCP 服务器 '{}' 未连接",
                    self.server_name
                )));
            }
            client
                .call_tool(&self.tool_info.name, arguments.clone())
                .await
        };

        match call_result {
            Ok(mcp_result) => {
                let output = mcp_result
                    .content
                    .iter()
                    .filter_map(|block| {
                        if block.content_type == "text" {
                            block.text.clone()
                        } else if block.content_type == "image" {
                            Some(format!(
                                "[image: {}]",
                                block.mime_type.as_deref().unwrap_or("unknown")
                            ))
                        } else if block.content_type == "resource" {
                            Some(format!(
                                "[resource: {}]",
                                block.text.as_deref().unwrap_or("unknown")
                            ))
                        } else {
                            block.text.clone()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");

                Ok(ToolResult {
                    success: !mcp_result.is_error,
                    output,
                    error: if mcp_result.is_error {
                        Some("MCP 工具返回错误".into())
                    } else {
                        None
                    },
                })
            }
            Err(e) => Ok(ToolResult::err(format!("MCP 工具调用失败: {}", e))),
        }
    }

    fn effect_kind(&self) -> EffectKind {
        EffectKind::Composite
    }

    fn risk_level(&self, _args: &Value) -> RiskLevel {
        RiskLevel::High
    }

    fn requires_approval_hint(&self, _args: &Value) -> bool {
        true
    }

    fn to_definition(&self) -> ToolDefinition {
        let desc = format!(
            "[MCP:{}] {}",
            self.server_name,
            self.tool_info.description.as_deref().unwrap_or("MCP tool")
        );
        ToolDefinition::new(&self.prefixed_name, &desc, self.parameters())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::client::McpToolInfo;
    use serde_json::json;

    #[test]
    fn test_prefixed_name() {
        let name = McpToolBridge::prefixed_name("filesystem", "read_file");
        assert_eq!(name, "mcp__filesystem__read_file");
    }

    #[test]
    fn test_parse_prefixed_name() {
        let result = McpToolBridge::parse_prefixed_name("mcp__filesystem__read_file");
        assert_eq!(result, Some(("filesystem".into(), "read_file".into())));
    }

    #[test]
    fn test_parse_prefixed_name_invalid() {
        assert_eq!(McpToolBridge::parse_prefixed_name("read_file"), None);
        assert_eq!(McpToolBridge::parse_prefixed_name("mcp__read_file"), None);
    }

    #[test]
    fn bridge_name_matches_definition_and_is_conservative() {
        let manager = Arc::new(Mutex::new(McpManager::new()));
        let bridge = McpToolBridge::new(
            "fs".to_string(),
            McpToolInfo {
                name: "read_file".to_string(),
                description: Some("read".to_string()),
                input_schema: None,
            },
            manager,
        );

        assert_eq!(bridge.name(), "mcp__fs__read_file");
        assert_eq!(bridge.to_definition().function.name, "mcp__fs__read_file");
        assert_eq!(bridge.effect_kind(), EffectKind::Composite);
        assert_eq!(bridge.risk_level(&json!({})), RiskLevel::High);
        assert!(bridge.requires_approval_hint(&json!({})));
    }
}
