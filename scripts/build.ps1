param(
    [switch]$Run
)

$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')

& python scripts\make-icon.py
if ($LASTEXITCODE -ne 0) {
    throw "icon generation failed with exit code $LASTEXITCODE"
}

& cargo build --release -p wake
if ($LASTEXITCODE -ne 0) {
    throw "cargo build failed with exit code $LASTEXITCODE"
}

$dist = Join-Path (Get-Location) 'dist'
New-Item -ItemType Directory -Force -Path $dist | Out-Null
$binary = Join-Path (Get-Location) 'target\release\Wake.exe'
$output = Join-Path $dist 'Wake.exe'
Copy-Item -LiteralPath $binary -Destination $output -Force

Write-Host "Built $output"
if ($Run) {
    Start-Process -FilePath $output -WorkingDirectory $dist
}
