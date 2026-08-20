param(
    [switch]$SkipBuild
)

$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')

function Invoke-Checked {
    param(
        [string]$Command,
        [string[]]$Arguments
    )

    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Command failed with exit code $LASTEXITCODE"
    }
}

$root = Get-Location
$dist = Join-Path $root 'dist'
$icon = Join-Path $root 'crates\wake\assets\icon.ico'
$binary = Join-Path $root 'target\release\Wake.exe'
$version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1).Matches.Groups[1].Value
$installer = Join-Path $dist ("Wake-$version-x64.msi")

Invoke-Checked 'python' @('scripts\make-icon.py')

if (-not $SkipBuild) {
    & (Join-Path $root 'scripts\build.ps1')
    if ($LASTEXITCODE -ne 0) {
        throw "scripts/build.ps1 failed with exit code $LASTEXITCODE"
    }
}

if (-not (Test-Path -LiteralPath $binary)) {
    throw "Release binary not found: $binary"
}

$wixCommand = Get-Command wix -ErrorAction SilentlyContinue
if ($wixCommand) {
    $wixTool = $wixCommand.Source
} else {
    $wixRoot = Join-Path $env:TEMP 'wake-wix-tool'
    $wixTool = Join-Path $wixRoot 'wix.exe'
    if (-not (Test-Path -LiteralPath $wixTool)) {
        Invoke-Checked 'dotnet' @(
            'tool', 'install', 'wix', '--tool-path', $wixRoot, '--version', '6.0.2'
        )
    }
}

New-Item -ItemType Directory -Force -Path $dist | Out-Null
Invoke-Checked $wixTool @(
    'build',
    'installer\Wake.wxs',
    '-arch', 'x64',
    '-d', "Version=$version",
    '-d', "WakeExe=$binary",
    '-d', "IconPath=$icon",
    '-o', $installer
)

Write-Host "Created $installer"
