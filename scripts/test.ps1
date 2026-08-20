param(
    [switch]$Smoke
)

$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')

function Invoke-Cargo {
    param([string[]]$Arguments)
    & cargo @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "cargo $($Arguments -join ' ') failed with exit code $LASTEXITCODE"
    }
}

Write-Host '--- cargo test -p wake-core'
Invoke-Cargo @('test', '-p', 'wake-core')

Write-Host '--- cargo check -p wake'
Invoke-Cargo @('check', '-p', 'wake')

if ($Smoke) {
    Write-Host '--- scan smoke test'
    Invoke-Cargo @('run', '-p', 'wake-core', '--bin', 'scan', '--', '--quiet')
}

Write-Host 'All checks passed.'
