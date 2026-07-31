# Movix 演示动图录制脚本
# 工具:https://github.com/charmbracelet/vhs
#
# 用法:
#   1. 安装 vhs(brew install vhs / scoop install vhs)
#   2. 在仓库根目录跑:vhs docs/demo.tape
#   3. 输出 docs/demo.gif(约 10 秒,~2-3 MB)
#
# 注意:
#   - 录制前请先 cargo build --release,确保 movix 二进制在 PATH
#   - vhs 会在指定 Sleep 处真实等待,因此总时长 ≈ 所有 Sleep 之和
#   - 录出来看到的是真终端,不是 SVG 模拟

Output docs/demo.gif

# ── 终端设置 ─────────────────────────────────────
Set Shell           bash
Set FontSize        14
Set Width           1400
Set Height          850
Set Padding         20
Set Theme           "Tokyo Night"
Set TypingSpeed     50ms
Set CursorBlink     true

# ── 启动 Movix ──────────────────────────────────
Type "movix"
Sleep 500ms
Enter
Sleep 2s

# 此时应看到欢迎界面(MOVIX 标题 + 快捷键速查)

# ── 提个问题(展示流式响应)───────────────────────
Type "列出当前目录的 3 个最重要的文件,并用一句话说明每个文件的作用"
Sleep 300ms
Enter

# 等待响应(实际跑要 5-15 秒,演示版短一点)
Sleep 8s

# ── 滚动到底部(展示 g 快捷键 + 流式结束)──────────
Type "g"
Sleep 1s

# ── 优雅退出(避免截到 Reset 残影)────────────────
Ctrl+C
Sleep 500ms
