#!/usr/bin/env bash
# Movix 一键安装脚本(Linux / macOS / WSL)
#
# 用法:
#   curl -fsSL https://raw.githubusercontent.com/Tinxyoo/Movix/main/install.sh | bash
#   curl -fsSL https://raw.githubusercontent.com/Tinxyoo/Movix/main/install.sh | bash -s -- --version v0.1.0
#   curl -fsSL https://raw.githubusercontent.com/Tinxyoo/Movix/main/install.sh | bash -s -- --to ~/.local/bin
#
# 选项:
#   -v, --version VER     指定版本(默认: 最新 release)
#   -t, --to DIR          安装目录(默认: /usr/local/bin,若无写权限则用 ~/.local/bin)
#   -r, --repo OWNER/REPO 仓库(默认: Tinxyoo/Movix)
#   -h, --help            显示帮助

set -euo pipefail

# curl 加固:重定向仅允许 https、连接/传输超时 60s、失败重试 3 次
CURL_HARDEN=(--proto-redir =https --connect-timeout 60 --max-time 300 --retry 3)

REPO="Tinxyoo/Movix"
VERSION=""
INSTALL_DIR=""
DRY_RUN=false

# ── 副作用包装器(dry-run 时只打印不执行)────────────────────
run() {
  if [[ "$DRY_RUN" == true ]]; then
    echo -e "  \033[36m[DRY-RUN]\033[0m $*"
  else
    "$@"
  fi
}

# ── 参数解析 ──────────────────────────────────────────────────────
print_help() {
  sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'
  exit 0
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    -v|--version) VERSION="$2"; shift 2;;
    -t|--to) INSTALL_DIR="$2"; shift 2;;
    -r|--repo) REPO="$2"; shift 2;;
    -n|--dry-run) DRY_RUN=true; shift;;
    -h|--help) print_help;;
    *) echo "未知参数: $1" >&2; exit 1;;
  esac
done

# ── 工具检测 ──────────────────────────────────────────────────────
command -v curl >/dev/null 2>&1 || { echo "❌ 缺少 curl,请先安装"; exit 1; }
command -v tar  >/dev/null 2>&1 || { echo "❌ 缺少 tar,请先安装"; exit 1; }
command -v uname >/dev/null 2>&1 || { echo "❌ 缺少 uname"; exit 1; }

# ── 平台/架构识别 ──────────────────────────────────────────────────
OS=$(uname -s)
ARCH=$(uname -m)

case "$OS" in
  Linux)  TARGET_TRIPLE="x86_64-unknown-linux-gnu" ;;
  Darwin) TARGET_TRIPLE="x86_64-apple-darwin" ;;
  *)      echo "❌ 不支持的操作系统: $OS(目前仅支持 Linux / macOS)"; exit 1;;
esac

case "$ARCH" in
  x86_64|amd64) ;;
  arm64|aarch64)
    if [[ "$OS" == "Darwin" ]]; then
      TARGET_TRIPLE="aarch64-apple-darwin"
    else
      TARGET_TRIPLE="aarch64-unknown-linux-gnu"
    fi
    ;;
  *) echo "❌ 不支持的架构: $ARCH"; exit 1;;
esac

# ── 安装目录兜底 ──────────────────────────────────────────────────
if [[ -z "$INSTALL_DIR" ]]; then
  if [[ -w "/usr/local/bin" ]]; then
    INSTALL_DIR="/usr/local/bin"
  else
    INSTALL_DIR="$HOME/.local/bin"
    mkdir -p "$INSTALL_DIR"
    echo "ℹ️  /usr/local/bin 无写权限,改用 $INSTALL_DIR"
    echo "   请把 $INSTALL_DIR 加入 PATH(若尚未):"
    echo "     export PATH=\"\$HOME/.local/bin:\$PATH\""
  fi
fi

# ── 版本获取 ──────────────────────────────────────────────────────
if [[ -z "$VERSION" ]]; then
  echo "🔍 正在获取最新版本..."
  if [[ "$DRY_RUN" == true ]]; then
    VERSION="v0.0.0-dryrun"
  else
    VERSION=$(curl -fsSL "${CURL_HARDEN[@]}" \
              -H "User-Agent: movix-installer" \
              "https://api.github.com/repos/$REPO/releases/latest" \
              | grep -oE '"tag_name":\s*"v?[^"]+"' | head -1 | sed -E 's/.*"v?([^"]+)".*/v\1/')
    if [[ -z "$VERSION" ]]; then
      echo "❌ 无法获取最新版本,请用 --version 手动指定"; exit 1
    fi
  fi
fi

VERSION_NUM="${VERSION#v}"
ASSET="movix-${TARGET_TRIPLE}.tar.gz"
URL="https://github.com/$REPO/releases/download/$VERSION/$ASSET"

# ── 下载 + 校验 ───────────────────────────────────────────────────
TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

echo "⬇️  下载 $ASSET ($VERSION)..."
run curl -fL "${CURL_HARDEN[@]}" --progress-bar -o "$TMPDIR/$ASSET" "$URL"

echo "🔐 校验 SHA-256..."
CHECKSUM_URL="https://github.com/$REPO/releases/download/$VERSION/$ASSET.sha256"
# 修复(R14):原实现校验文件下载失败时仅 warn 后跳过,MITM/DNS 劫持让 .sha256 返回 404
# 即可旁路整个校验,binary 无校验地 mv 到 /usr/local/bin(供应链降级)。
# 改为:校验文件缺失 → 硬失败退出,除非用户显式设置 MOVIX_SKIP_CHECKSUM=1 并确认风险。
if run curl -fsSL "${CURL_HARDEN[@]}" -o "$TMPDIR/$ASSET.sha256" "$CHECKSUM_URL" 2>/dev/null; then
  # macOS 默认无 sha256sum,回退到 shasum -a 256
  if command -v sha256sum >/dev/null 2>&1; then
    SHA_CMD=(sha256sum -c)
  else
    SHA_CMD=(shasum -a 256 -c)
  fi
  if [[ "$DRY_RUN" != true ]]; then
    (cd "$TMPDIR" && "${SHA_CMD[@]}" "$ASSET.sha256")
  else
    echo -e "  \033[36m[DRY-RUN]\033[0m (cd $TMPDIR && ${SHA_CMD[*]} $ASSET.sha256)"
  fi
else
  if [[ "${MOVIX_SKIP_CHECKSUM:-0}" == "1" ]]; then
    echo -e "  \033[33m⚠️  未找到 SHA-256 校验文件,已按 MOVIX_SKIP_CHECKSUM=1 跳过(不推荐,存在供应链风险)\033[0m"
  else
    echo -e "  \033[31m✖ 校验文件下载失败($CHECKSUM_URL)。为防止安装被篡改的二进制,已中止。\033[0m" >&2
    echo -e "    若你确信网络环境可信且需要跳过校验,设置 MOVIX_SKIP_CHECKSUM=1 后重试。" >&2
    exit 1
  fi
fi

# ── 解压 + 安装 ───────────────────────────────────────────────────
echo "📦 解压..."
# --no-same-permissions:剥离 tar 中潜在的 setuid/setgid 位(防提权)
run tar --no-same-permissions -xzf "$TMPDIR/$ASSET" -C "$TMPDIR"

# 断言 movix 是常规文件而非符号链接(防 symlink 攻击);dry-run 跳过
if [[ "$DRY_RUN" != true ]]; then
  if [[ ! -f "$TMPDIR/movix" || -L "$TMPDIR/movix" ]]; then
    echo "❌ 解压结果异常:movix 不存在或为符号链接" >&2
    exit 1
  fi
fi

run chmod +x "$TMPDIR/movix"

# macOS Gatekeeper:默认保留 quarantine 标记,让首次运行由系统提示用户确认。
# 若需无交互安装(如 CI),显式设置 MOVIX_STRIP_QUARANTINE=1。
if [[ "$OS" == "Darwin" && "${MOVIX_STRIP_QUARANTINE:-0}" == "1" ]]; then
  echo "ℹ️  已按 MOVIX_STRIP_QUARANTINE=1 剥离 macOS quarantine 标记"
  run xattr -dr com.apple.quarantine "$TMPDIR/movix" 2>/dev/null || true
fi

echo "🚀 安装到 $INSTALL_DIR/movix..."
if [[ "$DRY_RUN" == true ]]; then
  echo -e "  \033[36m[DRY-RUN]\033[0m mv $TMPDIR/movix $INSTALL_DIR/movix"
else
  # 优先使用 mv,失败则请求 sudo(显式告知来源与校验状态)
  if mv "$TMPDIR/movix" "$INSTALL_DIR/movix" 2>/dev/null; then
    :
  else
    echo "ℹ️  $INSTALL_DIR 需要提权,即将以 root 执行:"
    echo "   sudo mv $TMPDIR/movix $INSTALL_DIR/movix"
    echo "   来源: $URL(已通过 SHA-256 校验)"
    sudo mv "$TMPDIR/movix" "$INSTALL_DIR/movix"
  fi
fi

# ── 验证 ─────────────────────────────────────────────────────────
echo ""
echo "✅ 安装完成!"
echo ""
if [[ "$DRY_RUN" == true ]]; then
  echo "(dry-run 模式,未实际执行;真实执行时会验证: $INSTALL_DIR/movix --version)"
else
  echo "验证: $("$INSTALL_DIR/movix" --version)"
fi
echo ""
echo "使用: $INSTALL_DIR/movix"
echo "首次运行会引导你输入 DeepSeek API Key。"
