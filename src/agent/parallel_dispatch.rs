//! ⚠️ **H1 维护警告(对抗式审查 2026-07)**:本模块 `ParallelDispatcher` 在全仓库
//! **无任何调用方**(主循环走 `agent/mod.rs::handle_tool_call` 的串行路径)。
//! 这里的并行 fan-out 逻辑是**死代码**,且其中关于"读工具无副作用竞争"的假设
//! 对 web 工具(共享全局 WEB_CLIENT、DuckDuckGo 限流)不成立。切流前需重新评估。
//! 保留以备未来接线;`#![allow(dead_code)]` 已设。

#![allow(dead_code)]

use crate::common::deepseek::ToolCall;
use crate::tools::{ToolRegistry, ToolResult};
use std::sync::Arc;
use tokio::task::JoinSet;

type ArgRepairFn = Arc<dyn Fn(&str) -> (serde_json::Value, bool) + Send + Sync>;

pub struct ParallelDispatcher {
    registry: Arc<ToolRegistry>,
    max_concurrency: usize,
}

impl ParallelDispatcher {
    pub fn new(registry: Arc<ToolRegistry>) -> Self {
        Self {
            registry,
            max_concurrency: 4,
        }
    }

    pub fn with_max_concurrency(mut self, max: usize) -> Self {
        self.max_concurrency = max.clamp(1, 8);
        self
    }

    pub fn classify(&self, tool_calls: &[ToolCall]) -> DispatchPlan {
        let mut parallel: Vec<ToolCall> = Vec::new();
        let mut sequential: Vec<ToolCall> = Vec::new();

        for tc in tool_calls {
            let bucket = match self.registry.get(&tc.function.name) {
                Some(t) if t.parallel_safe() => &mut parallel,
                _ => &mut sequential,
            };
            bucket.push(tc.clone());
        }

        DispatchPlan {
            parallel,
            sequential,
        }
    }

    pub async fn dispatch(
        &self,
        tool_calls: &[ToolCall],
        arg_repair_fn: impl Fn(&str) -> (serde_json::Value, bool) + Send + Sync + 'static,
    ) -> Vec<DispatchResult> {
        // 修复(Medium #M2):原实现"先跑全部 parallel(读)、再跑全部 sequential(写)",
        // 破坏了原始调用顺序。若序列是 [write(x), read(x)],实际变成 [read旧, write],
        // 读操作看到修改前的旧内容。
        //
        // 正确策略:按原始顺序分段扫描——遇到 parallel_safe 的工具就攒进当前并发桶,
        // 遇到不并发安全的工具就先把当前桶里的读操作并发跑完,再串行执行该写操作,
        // 然后开新桶。这样既保留读操作的并发收益,又保证"写先于后续读"的依赖序。
        let repair_arc: ArgRepairFn = Arc::new(arg_repair_fn);
        let mut results: Vec<DispatchResult> = Vec::with_capacity(tool_calls.len());
        let mut bucket: Vec<ToolCall> = Vec::new();

        for tc in tool_calls {
            let parallel_safe = self
                .registry
                .get(&tc.function.name)
                .is_some_and(|t| t.parallel_safe());
            if parallel_safe {
                bucket.push(tc.clone());
            } else {
                // 先把桶里的读操作并发跑完,保证它们看到的是写操作之前的状态
                Self::flush(&self.registry, &repair_arc, &mut bucket, &mut results).await;
                // 再串行执行这个写操作
                results.push(Self::execute_one(&self.registry, tc, &repair_arc).await);
            }
        }
        // 收尾:跑完最后一桶读操作
        Self::flush(&self.registry, &repair_arc, &mut bucket, &mut results).await;

        // 按原始 call_id 排序,保证返回顺序与输入一致。
        results.sort_by(|a, b| a.call_id.cmp(&b.call_id));
        results
    }

    /// 把当前桶里的 parallel_safe 工具并发跑完,结果追加到 `results`。
    /// 桶为空则空操作。跑完清空桶。
    async fn flush(
        registry: &Arc<ToolRegistry>,
        repair_arc: &ArgRepairFn,
        bucket: &mut Vec<ToolCall>,
        results: &mut Vec<DispatchResult>,
    ) {
        let taken = std::mem::take(bucket);
        if taken.is_empty() {
            return;
        }
        if taken.len() == 1 {
            let tc = &taken[0];
            results.push(Self::execute_one(registry, tc, repair_arc).await);
        } else {
            // dispatch_parallel 需要 Arc clone(它内部 spawn task)。
            // 这里构造一个临时 dispatcher 复用其并发逻辑。
            let dispatcher = ParallelDispatcher {
                registry: registry.clone(),
                max_concurrency: 4,
            };
            results.extend(
                dispatcher
                    .dispatch_parallel(&taken, repair_arc.clone())
                    .await,
            );
        }
    }

    /// 真正跑一个工具调用:查注册表、repair 参数、执行。失败/未知工具都返回结构化错误。
    /// 单独抽出来,串行分支和并行分支共用,避免逻辑分叉;接收 `&Arc<ToolRegistry>`
    /// 而非 `&self`,这样能塞进 `'static` 的 JoinSet task。
    async fn execute_one(
        registry: &Arc<ToolRegistry>,
        tc: &ToolCall,
        arg_repair_fn: &ArgRepairFn,
    ) -> DispatchResult {
        let (args, _repaired) = arg_repair_fn(&tc.function.arguments);

        let result = match registry.get(&tc.function.name) {
            Some(tool) => tool
                .execute(&args)
                .await
                .unwrap_or_else(|e| ToolResult::err(format!("执行错误: {}", e))),
            None => ToolResult::err(format!("未知工具: {}", tc.function.name)),
        };

        DispatchResult {
            call_id: tc.id.clone(),
            tool_name: tc.function.name.clone(),
            result,
        }
    }

    async fn dispatch_parallel(
        &self,
        calls: &[ToolCall],
        arg_repair_fn: ArgRepairFn,
    ) -> Vec<DispatchResult> {
        let mut all_results = Vec::new();

        for chunk in calls.chunks(self.max_concurrency) {
            let mut set = JoinSet::new();
            // 记录 task::Id → (call_id, tool_name) 映射，
            // 用于 JoinError 时关联原始调用
            let mut task_info: std::collections::HashMap<tokio::task::Id, (String, String)> =
                std::collections::HashMap::new();

            for tc in chunk {
                let registry = Arc::clone(&self.registry);
                let tc_clone = tc.clone();
                let repair = Arc::clone(&arg_repair_fn);
                let call_id = tc.id.clone();
                let tool_name = tc.function.name.clone();
                let handle = set
                    .spawn(async move { Self::execute_one(&registry, &tc_clone, &repair).await });
                task_info.insert(handle.id(), (call_id, tool_name));
            }

            while let Some(res) = set.join_next_with_id().await {
                match res {
                    Ok((id, dr)) => {
                        // 正常完成：execute_one 已包含正确的 call_id/tool_name
                        task_info.remove(&id); // 清理映射
                        all_results.push(dr);
                    }
                    Err(e) => {
                        // JoinError：用 task Id 精确查找原始调用信息
                        let (call_id, tool_name) = task_info.remove(&e.id()).unwrap_or_default();
                        all_results.push(DispatchResult {
                            call_id,
                            tool_name,
                            result: ToolResult::err(format!("任务失败: {}", e)),
                        });
                    }
                }
            }
        }

        all_results
    }
}

#[derive(Debug, Clone)]
pub struct DispatchPlan {
    pub parallel: Vec<ToolCall>,
    pub sequential: Vec<ToolCall>,
}

#[derive(Debug, Clone)]
pub struct DispatchResult {
    pub call_id: String,
    pub tool_name: String,
    pub result: ToolResult,
}
