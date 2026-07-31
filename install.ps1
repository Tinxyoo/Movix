# Movix 一键安装脚本(Windows PowerShell 5.1+)
#
# 用法(在 PowerShell 中):
#   iwr -useb https://raw.githubusercontent.com/Tinxyoo/Movix/main/install.ps1 | iex
#   iwr -useb https://raw.githubusercontent.com/Tinxyoo/Movix/main/install.ps1 | iex -Version v0.1.0
#   iwr -useb https://raw.githubusercontent.com/Tinxyoo/Movix/main/install.ps1 | iex -InstallDir "$env:USERPROFILE\bin"
#
# 参数(通过 -<Name> 传递):
#   -Version       指定版本(默认: 最新 release)
#   -InstallDir    安装目录(默认: $env:USERPROFILE\bin)
#   -Repo          仓库(默认: Tinxyoo/Movix)

[CmdletBinding()]
param(
    [string]$Version = "",
    [string]$InstallDir = "",
    [string]$Repo = "Tinxyoo/Movix"
)

$ErrorActionPreference = 'Stop'

# 强制 TLS 1.2+(GitHub 要求;PowerShell 5.1 默认可能为 TLS 1.0/1.1)
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

# ── 帮助 ─────────────────────────────────────────────────────────
function Show-Help {
    @'
Movix 一键安装脚本 (Windows PowerShell 5.1+)

参数:
  -Version     指定版本(默认: 最新 release)
  -InstallDir  安装目录(默认: $env:USERPROFILE\bin)
  -Repo        仓库(默认: Tinxyoo/Movix)

示例:
  iwr -useb https://raw.githubusercontent.com/Tinxyoo/Movix/main/install.ps1 | iex
  iwr -useb ... | iex -Version v0.1.0
  iwr -useb ... | iex -InstallDir "C:\tools"
'@
}

if ($MyInvocation.InvocationName -eq '&' -and $args[0] -eq '-h') {
    Show-Help; return
}

# ── 工具检测 ─────────────────────────────────────────────────────
if (-not (Get-Command Invoke-WebRequest -ErrorAction SilentlyContinue)) {
    Write-Error "❌ PowerShell 5.1+ 是必需的(需要 Invoke-WebRequest / Expand-Archive)"
    exit 1
}
if (-not (Get-Command Expand-Archive -ErrorAction SilentlyContinue)) {
    Write-Error "❌ 缺少 Expand-Archive(Win10 1607+ 自带)"
    exit 1
}

# ── 架构 ─────────────────────────────────────────────────────────
$arch = $env:PROCESSOR_ARCHITECTURE
if ($arch -ne 'AMD64' -and $arch -ne 'x86_64') {
    Write-Error "❌ 不支持架构: $arch(目前仅 x64)"
    exit 1
}
$TargetTriple = 'x86_64-pc-windows-msvc'
$Asset        = "movix-$TargetTriple.zip"

# ── 安装目录 ─────────────────────────────────────────────────────
if ([string]::IsNullOrEmpty($InstallDir)) {
    $InstallDir = Join-Path $env:USERPROFILE 'bin'
}
if (-not (Test-Path $InstallDir)) {
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
}

# ── 版本获取 ─────────────────────────────────────────────────────
if ([string]::IsNullOrEmpty($Version)) {
    Write-Host "🔍 正在获取最新版本..." -ForegroundColor Cyan
    $apiUrl = "https://api.github.com/repos/$Repo/releases/latest"
    $release = Invoke-RestMethod -Uri $apiUrl -Headers @{ 'User-Agent' = 'movix-installer' }
    $Version = $release.tag_name
    if ([string]::IsNullOrEmpty($Version)) {
        Write-Error "❌ 无法获取最新版本,请用 -Version 手动指定"
        exit 1
    }
}

$DownloadUrl = "https://github.com/$Repo/releases/download/$Version/$Asset"
$ChecksumUrl = "https://github.com/$Repo/releases/download/$Version/$Asset.sha256"

# ── 临时目录 ─────────────────────────────────────────────────────
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("movix-install-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
    $zipPath  = Join-Path $tmp $Asset
    $checkPath = Join-Path $tmp "$Asset.sha256"

    # ── 下载 ─────────────────────────────────────────────────────
    Write-Host "⬇️  下载 $Asset ($Version)..." -ForegroundColor Cyan
    Invoke-WebRequest -Uri $DownloadUrl -OutFile $zipPath -UseBasicParsing

    # ── 校验 ─────────────────────────────────────────────────────
    # 与 install.sh 对齐:校验文件缺失 → 硬失败退出,防止 MITM/CDN 对 .sha256 返回 404
    # 即可旁路校验、装上被篡改的二进制(供应链降级)。仅在用户显式设置
    # MOVIX_SKIP_CHECKSUM=1 并接受风险时才跳过。
    Write-Host "🔐 校验 SHA-256..." -ForegroundColor Cyan
    $skipChecksum = ($env:MOVIX_SKIP_CHECKSUM -eq '1')
    try {
        Invoke-WebRequest -Uri $ChecksumUrl -OutFile $checkPath -UseBasicParsing -ErrorAction Stop
        $expected = (Get-Content $checkPath | Select-Object -First 1).Split()[0].ToLower()
        $actual   = (Get-FileHash -Path $zipPath -Algorithm SHA256).Hash.ToLower()
        if ($expected -ne $actual) {
            Write-Error "❌ SHA-256 不匹配`n  期望: $expected`n  实际: $actual"
            exit 1
        }
        Write-Host "✅ SHA-256 校验通过" -ForegroundColor Green
    } catch {
        if ($skipChecksum) {
            Write-Warning "⚠️  未找到 SHA-256 校验文件,已按 MOVIX_SKIP_CHECKSUM=1 跳过(不推荐,存在供应链风险)"
        } else {
            Write-Error "❌ 校验文件下载失败($ChecksumUrl)。为防止安装被篡改的二进制,已中止。"
            Write-Host "    若你确信网络环境可信且需要跳过校验,设置 MOVIX_SKIP_CHECKSUM=1 后重试。" -ForegroundColor Yellow
            exit 1
        }
    }

    # ── 解压(先校验 zip 条目路径,防穿越)──────────────────────
    Write-Host "📦 解压..." -ForegroundColor Cyan
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $archive = [System.IO.Compression.ZipFile]::OpenRead($zipPath)
    try {
        $tmpRoot = (Resolve-Path $tmp).Path.TrimEnd('\') + '\'
        foreach ($entry in $archive.Entries) {
            # 规范化分隔符后拼接,再解析绝对路径判断是否逃出 $tmp
            $entryPath = $entry.FullName -replace '/', '\'
            $dest = Join-Path $tmp $entryPath
            $resolved = [System.IO.Path]::GetFullPath($dest)
            if (-not $resolved.StartsWith($tmpRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
                Write-Error "❌ 检测到 zip 条目路径穿越: $($entry.FullName)"
                exit 1
            }
        }
    } finally {
        $archive.Dispose()
    }
    Expand-Archive -Path $zipPath -DestinationPath $tmp -Force
    $exe = Join-Path $tmp 'movix.exe'
    if (-not (Test-Path $exe)) {
        Write-Error "❌ 压缩包内未找到 movix.exe"
        exit 1
    }

    # ── 安装 ─────────────────────────────────────────────────────
    $target = Join-Path $InstallDir 'movix.exe'
    Write-Host "🚀 安装到 $target ..." -ForegroundColor Cyan
    Move-Item -Path $exe -Destination $target -Force

    # ── PATH 检查 ────────────────────────────────────────────────
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ($userPath -notlike "*$InstallDir*") {
        Write-Host ""
        Write-Host "⚠️  $InstallDir 不在当前用户的 PATH 中" -ForegroundColor Yellow
        $add = Read-Host "是否自动加入用户 PATH?(y/N)"
        if ($add -in @('y','Y','yes','Yes')) {
            [Environment]::SetEnvironmentVariable(
                'Path',
                "$userPath;$InstallDir",
                'User'
            )
            $env:Path = "$env:Path;$InstallDir"
            Write-Host "✅ 已加入用户 PATH(新打开的终端生效)" -ForegroundColor Green
        }
    }

    # ── 验证 ─────────────────────────────────────────────────────
    Write-Host ""
    Write-Host "✅ 安装完成!" -ForegroundColor Green
    Write-Host ""
    & $target --version
    Write-Host ""
    Write-Host "使用: movix" -ForegroundColor Cyan
    Write-Host "首次运行会引导你输入 DeepSeek API Key。"
}
finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
