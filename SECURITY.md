# Security Policy / 安全策略

## Supported Versions / 支持版本

Movix 处于早期开发阶段(v0.1.x)。安全修复仅针对最新 release。

| Version | Supported |
|---------|-----------|
| 最新 release | ✅ |
| 低于最新 | ❌ |

## Reporting a Vulnerability / 漏洞上报

请**不要**在公开 Issue 中披露安全漏洞。

报告方式(任选其一):

1. **首选** — GitHub Private Vulnerability Reporting:仓库 → Security 标签 → Report a vulnerability
2. 邮件:[dremo@qq.com](mailto:dremo@qq.com),主题加 `[SEC]` 前缀

请在报告中包含:

- 受影响版本(`movix --version`)
- 复现步骤(最小可复现示例)
- 影响评估(能否 RCE / 数据泄露 / 提权)
- 建议修复方向(可选)

**响应预期**:3 个工作日内确认收到,7 个工作日内给出初步评估。修复时间视严重度而定。

## Security Model / 安全边界

Movix 是一个能**执行 Shell 命令、读写文件、发起网络请求、连接外部 MCP 服务器**的 AI 编程 Agent。使用前请理解以下边界。

### Movix 提供的防护

- **路径越界检查** — 防止工具访问工作区外的文件
- **敏感文件保护** — `.env`、SSH 密钥等读取受限(读取同样受限,不止写入)
- **危险命令拦截** — 启发式识别 `rm -rf /`、`curl | sh` 等危险操作(含长选项 / 反斜杠 / `~` 等绕过变体)
- **出网控制** — shell 工具默认禁止 `curl` / `git push` 等外发(`MOVIX_ALLOW_SHELL_EGRESS=1` 才放开)
- **模式级审批** — Plan / Agent 模式需用户确认写操作;Auto / YOLO 自动执行
- **MCP 信任门** — 工作区 `.movix/mcp.json` 默认不自动连接(需 `MOVIX_TRUST_MCP_FILE=1`),防止恶意仓库自证 `trusted:true` 即 RCE
- **Snapshot 回滚** — 写文件前建内容寻址快照,失败时可回滚(仅限 agent 实际修改过的文件)

### Movix **不**提供的防护

- **非 OS 级隔离** — Sandbox 是启发式风险扫描,不是沙箱。不可信任务请在 Docker / 虚拟机 / `bubblewrap` 中运行
- **Prompt 注入免疫** — Auto / YOLO 模式下打开陌生仓库的 skill(`.movix/skills`)、`AGENT.md`、`.movix/mcp.json` 仍可能被 prompt 注入利用。技能内容仅以 `<untrusted>` 标签作为**软提示**注入 LLM,无执行隔离;真正防线是 `use_skill` 的强制审批
- **网络隔离** — `web_search` / `web_fetch` 工具可主动出网;MCP 服务器连接后亦可出网
- **并发安全** — agent 运行时若用户在终端外并发修改文件,Snapshot 不会全量回滚你的并发改动(仅回滚 agent 改过的文件)

### 安装脚本

`install.sh` / `install.ps1` 通过 SHA-256 校验防止二进制在传输 / CDN 缓存环节被篡改。但校验文件与二进制同源下载自同一 GitHub Release,**无法抵御 release 本身被攻陷**(如维护者 token 泄露)。详见仓库源码注释中的 R14 修复说明。

## Out of Scope / 不在范围内的问题

以下情形**不应**作为安全漏洞上报:

- 在 YOLO 模式下执行用户已明确批准的危险命令
- 在用户主动设置 `MOVIX_ALLOW_SHELL_EGRESS=1` 后的外发行为
- 在用户主动设置 `MOVIX_TRUST_MCP_FILE=1` 后连接恶意 MCP 服务器导致的后果
- 在用户主动设置 `MOVIX_SKIP_CHECKSUM=1` / `MOVIX_STRIP_QUARANTINE=1` 后的供应链风险
- 任何需要先获得代码执行权才能触发的提权(已 RCE 后的横向移动)
- 通过修改本地 `~/.movix/` 配置文件实现的攻击(本地用户即信任边界)
- 通过环境变量 / CLI 参数注入的攻击(env / flags 视为可信输入)

## Disclosure / 披露

- 修复发布后,会在 Release Notes 中致谢报告者(除非要求匿名)
- 严重漏洞修复后会发布 GitHub Security Advisory
