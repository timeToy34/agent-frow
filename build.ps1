param(
    [Parameter(Mandatory = $true)][string]$OutputRoot,
    [ValidateSet('Build', 'Test', 'Install', 'Package')][string]$Mode = 'Install'
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'scripts\build-support.ps1')
$context = New-BuildContext -SourceRoot $PSScriptRoot -OutputRoot $OutputRoot
$oldTarget = $env:CARGO_TARGET_DIR
Push-Location -LiteralPath $context.Source
try {
    $env:CARGO_TARGET_DIR = $context.Target
    if ($Mode -eq 'Package') { Assert-NewPackage $context }
    # CI uses explicit Test/Build modes and never touches an installed app.
    if ($Mode -in @('Test', 'Install', 'Package')) {
        Invoke-Native cargo @('test', '--workspace', '--locked')
    }
    if ($Mode -ne 'Test') {
        Invoke-Native cargo @('build', '--release', '--workspace', '--locked')
        Write-BuildMetadata $context
    }
    switch ($Mode) {
        'Install' { Install-TestBuild $context }
        'Package' { Publish-Package $context }
        'Build' { Write-Output "Release build: $($context.Release)" }
    }
} finally {
    $env:CARGO_TARGET_DIR = $oldTarget
    Pop-Location
}
