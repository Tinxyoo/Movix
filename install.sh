#!/usr/bin/env bash
# Movix 一键安装脚本(Linux / macOS / WSL)
#
# 用法:
#   curl -fsSL https://raw.githubusercontent.com/Tinxyoo/Movix/main/install.sh | bash
#
# 脚本会自动检测 Rust 工具链,若未安装则通过 rustup 安装,
# 然后用 cargo install --git 编译并安装 movix 到 ~/.cargo/bin。

set -euo pipefail

REPO="Tinxyoo/Movix"

# 颜色
GREEN='\033[0;32m'
CYAN='\033[0;36m'
YELLOW='\033[0;33m'
RED='\033[0;31m'
NC='\033[0m'

echo -e "${CYAN}════════════════════════════════════════${NC}"
echo -e "${CYAN}  Movix Installer${NC}"
echo -e "${CYAN}════════════════════════════════════════${NC}"
echo ""

# ── 检测 Rust / cargo ────────────────────────────────────────────
if command -v cargo >/dev/null 2>&1; then
  echo -e "${GREEN}✓${NC} 已检测到 Rust 工具链: $(rustc --version)"
else
  echo -e "${YELLOW}⚠${NC} 未检测到 Rust,正在通过 rustup 安装..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  # 加载 cargo 环境变量
  source "$HOME/.cargo/env"
  echo -e "${GREEN}✓${NC} Rust 安装完成: $(rustc --version)"
fi

# ── 编译安装 ──────────────────────────────────────────────────────
echo ""
echo -e "${CYAN}⬇️  正在编译安装 movix(可能需要几分钟)...${NC}"
cargo install --git "https://github.com/$REPO" --locked

# ── 验证 ──────────────────────────────────────────────────────────
echo ""
echo -e "${GREEN}✅ 安装完成!${NC}"
echo ""
echo "版本: $(movix --version)"
echo ""
echo "使用: movix"
echo "首次运行会引导你输入 DeepSeek API Key。"
