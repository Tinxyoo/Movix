# 截图与录屏

本目录存放 README 引用的视觉素材。

## 截图

| 文件 | 内容 | 来源 |
|------|------|------|
| `welcome.png` | 启动界面（MOVIX 标题 + 快捷键速查 + 三栏布局）| `cargo run` 后立即截图 |
| `chat.png` | 对话进行中（USER/Movix 气泡 + 推理块 + 工具调用）| 提个问题，等响应后截图 |
| `tools.png` | `movix tools` 命令输出 | 终端截图 |
| `plan-mode.png` | Plan 模式（多步任务规划）| 按 `Tab` 切到 Plan 后再提问 |
| `review.png` | 代码审查（Reviewer）输出 | 触发审查逻辑 |
| `diff.png` | `patch_file` 工具的 diff 渲染 | 修改文件后截图 |
| `cost.png` | 费用统计（侧边栏 TURNS 区域）| 多跑几轮后截图 |

## 录屏

| 文件 | 内容 | 生成方式 |
|------|------|----------|
| `demo.gif` | 10 秒演示（启动 → 提问 → 流式响应）| `vhs docs/demo.tape` |

## 重新生成 demo.gif

需要 [vhs](https://github.com/charmbracelet/vhs)（Charmbracelet 出的 TUI 录制工具）。

```bash
# 安装 vhs
brew install vhs           # macOS / Linux / WSL
scoop install vhs          # Windows

# 设置一个 dummy API key,让 movix 能进入 TUI 不报错
# (vhs 录制会在 API 调用处停 5 秒然后退出,不会真发请求)
export DEEPSEEK_API_KEY=sk-vhs-demo

# 生成
vhs docs/demo.tape
```

输出 `docs/demo.gif`，可直接提交到仓库。

## 截图建议

- **分辨率**：1400×800 起，保证 README 中显示清晰
- **字体**：等宽，Cascadia Code / JetBrains Mono / Fira Code
- **主题**：暗色（与 Movix 风格一致），Tokyo Night / Catppuccin Mocha / One Dark
- **不要 PII**：截图前清掉窗口标题里的用户名/路径
