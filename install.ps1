# Movix 一键安装脚本(Windows PowerShell 5.1+)
#
# 用法(在 PowerShell 中):
#   iwr -useb https://raw.githubusercontent.com/Tinxyoo/Movix/main/install.ps1 | iex
#
# 脚本会自动检测 Rust 工具链,若未安装则通过 rustup 安装,
# 然后用 cargo install --git 编译并安装 movix。

$ErrorActionPreference = 'Stop'

# 强制 TLS 1.2+(GitHub 要求)
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$Repo = "Tinxyoo/Movix"

Write-Host "════════════════════════════════════════" -ForegroundColor Cyan
Write-Host "  Movix Installer" -ForegroundColor Cyan
Write-Host "════════════════════════════════════════" -ForegroundColor Cyan
Write-Host ""

# ── 检测 Rust / cargo ────────────────────────────────────────────
$cargo = Get-Command cargo -ErrorAction SilentlyContinue
if ($cargo) {
    Write-Host "✓ 已检测到 Rust 工具链: $(rustc --version)" -ForegroundColor Green
} else {
    Write-Host "⚠ 未检测到 Rust,正在通过 rustup 安装..." -ForegroundColor Yellow
    $rustupInit = Join-Path $env:TEMP "rustup-init.exe"
    Invoke-WebRequest -Uri "https://win.rustup.rs/x86_64" -OutFile $rustupInit -UseBasicParsing
    & $rustupInit -y
    Remove-Item $rustupInit -Force

    # 加载 cargo 环境变量
    $cargoEnv = Join-Path $env:USERPROFILE ".cargo\env.ps1"
    if (Test-Path $cargoEnv) { . $cargoEnv }
    $env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"

    Write-Host "✓ Rust 安装完成: $(rustc --version)" -ForegroundColor Green
}

# ── 编译安装 ──────────────────────────────────────────────────────
Write-Host ""
Write-Host "⬇️  正在编译安装 movix(可能需要几分钟)..." -ForegroundColor Cyan
cargo install --git "https://github.com/$Repo" --locked

# ── 验证 ──────────────────────────────────────────────────────────
Write-Host ""
Write-Host "✅ 安装完成!" -ForegroundColor Green
Write-Host ""
& movix --version
Write-Host ""
Write-Host "使用: movix" -ForegroundColor Cyan
Write-Host "首次运行会引导你输入 DeepSeek API Key。"
