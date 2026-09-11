# 构建 Windows 发行产物:release 二进制(图标/版本资源由 build.rs 嵌入)
# 打成 zip。在 Windows 上跑(CI 或本机);macOS/Linux 打包各走
# make-app.sh / make-linux.sh,互不认识对方的产物。
# 用法: powershell -ExecutionPolicy Bypass -File scripts/make-windows.ps1
#
# assets/icon.ico 由 icon.svg 预生成入库,改了 SVG 后再生成(任一平台):
#   rsvg-convert -w 1024 -h 1024 crates/wake/assets/icon.svg -o /tmp/icon-1024.png
#   python3 -c "from PIL import Image; Image.open('/tmp/icon-1024.png').convert('RGBA').save(
#     'crates/wake/assets/icon.ico', sizes=[(16,16),(24,24),(32,32),(48,48),(64,64),(128,128),(256,256)])"

$ErrorActionPreference = "Stop"
Set-Location (Join-Path $PSScriptRoot "..")

$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.*)"').Matches[0].Groups[1].Value
# 架构取自实际构建目标(rustc 的 host triple),不是 PowerShell 进程的架构:
# 后者在 ARM64 机器上跑 x64 模拟壳时会报 AMD64,给 ARM64 产物贴上 x86_64 标签
$hostTriple = (rustc -vV | Select-String '^host: (.*)$').Matches[0].Groups[1].Value
$arch = if ($hostTriple -like "aarch64*") { "arm64" } else { "x86_64" }
$targetDir = if ($env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR } else { "target" }

cargo build --release -p wake
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
cargo build --release -p wake-core --bin wake-mcp --bin wake-cli
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# 二进制名 Wake.exe 来自 [[bin]](macOS 菜单栏取进程名所需);Windows 惯例
# 首字母大写正好同形,不再改名
$stage = Join-Path ([System.IO.Path]::GetTempPath()) "wake-stage\wake-$version-windows-$arch"
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Path $stage -Force | Out-Null
Copy-Item (Join-Path $targetDir "release/Wake.exe") (Join-Path $stage "Wake.exe")
# 只读 MCP server,与主程序并排(Settings → Connect 按同目录找它)
Copy-Item (Join-Path $targetDir "release/wake-mcp.exe") (Join-Path $stage "wake-mcp.exe")
# 会话查询 CLI,同样并排
Copy-Item (Join-Path $targetDir "release/wake-cli.exe") (Join-Path $stage "wake-cli.exe")

New-Item -ItemType Directory -Path dist -Force | Out-Null
$zip = "dist/wake-$version-windows-$arch.zip"
if (Test-Path $zip) { Remove-Item -Force $zip }
Compress-Archive -Path $stage -DestinationPath $zip

Get-Item $zip | ForEach-Object { "{0} ({1:N1} MB)" -f $_.FullName, ($_.Length / 1MB) }
