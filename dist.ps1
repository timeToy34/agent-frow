param([Parameter(Mandatory = $true)][string]$OutputRoot)

# Compatibility entry point: packaging uses the same Windows build workflow.
$ErrorActionPreference = 'Stop'
& (Join-Path $PSScriptRoot 'build.ps1') -OutputRoot $OutputRoot -Mode Package
