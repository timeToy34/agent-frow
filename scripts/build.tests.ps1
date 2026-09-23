# No external test framework or real app installation. All writes use TEMP.
$ErrorActionPreference = 'Stop'
$source = Split-Path -Parent $PSScriptRoot
. (Join-Path $PSScriptRoot 'build-support.ps1')
$scratch = Join-Path $env:TEMP ('agent-frow-workflow-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $scratch | Out-Null

function Assert-True([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}
function Assert-Rejected([scriptblock]$Work, [string]$Message) {
    try { & $Work | Out-Null }
    catch {
        if ($_.ToString() -notmatch $Message) { throw }
        return
    }
    throw "Expected rejection matching: $Message"
}

try {
    Assert-Rejected { New-BuildContext $source 'relative-output' } 'absolute path'
    Assert-Rejected { New-BuildContext $source 'C:\' } 'drive root'
    Assert-Rejected { New-BuildContext $source (Join-Path $env:LOCALAPPDATA 'agent-frow') } 'installed app'
    $duplicate = Join-Path $scratch 'old checkout'
    New-Item -ItemType Directory -Path (Join-Path $duplicate '.git') -Force | Out-Null
    Assert-Rejected { New-BuildContext $source $duplicate } 'Git checkout'
    Push-Location -LiteralPath $scratch
    try {
        Invoke-Native $env:ComSpec @('/d', '/c', 'exit 0')
        Assert-Rejected { Invoke-Native $env:ComSpec @('/d', '/c', 'exit 9') } 'exit code 9'
    } finally { Pop-Location }

    $context = New-BuildContext $source (Join-Path $scratch 'output with spaces')
    Assert-True ($context.Source -eq $source) 'Source root was not retained.'
    Assert-Rejected { New-BuildContext (Join-Path $source 'app') $context.Output } 'belongs to'
    New-Item -ItemType Directory -Path $context.Release -Force | Out-Null
    foreach ($name in @('agent-frow.exe', 'agent-frow-hook.exe')) {
        "new $name" | Set-Content -LiteralPath (Join-Path $context.Release $name)
    }
    Write-BuildMetadata $context
    $info = Get-Content -LiteralPath (Join-Path $context.Release 'build-info.json') -Raw | ConvertFrom-Json
    Assert-True ($info.commit -match '^[0-9a-f]{40}$') 'Missing Git identity.'
    foreach ($entry in $info.sha256.PSObject.Properties) {
        Assert-True ((Get-FileHash -LiteralPath (Join-Path $context.Release $entry.Name)).Hash -eq $entry.Value) 'Wrong artifact hash.'
    }
    Publish-Package $context
    $zip = Join-Path $context.Dist "agent-frow-$($context.Version)-win64.zip"
    $checksum = (Get-Content -LiteralPath "$zip.sha256").Split(' ')[0]
    Assert-True ((Get-FileHash -LiteralPath $zip).Hash -eq $checksum) 'Package checksum mismatch.'
    Assert-Rejected { Publish-Package $context } 'already exists'
    Assert-True ((Get-FileHash -LiteralPath $zip).Hash -eq $checksum) 'Existing package was changed.'
    Assert-True (@(Get-ChildItem -LiteralPath $context.Target -Filter 'package-*').Count -eq 0) 'Staging folder leaked.'
    $realSource = $context.Source
    $context.Source = Join-Path $scratch 'missing-source'
    $context.Version = '0.0.0-test'
    Assert-Rejected { Publish-Package $context } 'does not exist|cannot find'
    Assert-True (@(Get-ChildItem -LiteralPath $context.Target -Filter 'package-*').Count -eq 0) 'Failed packaging left staging behind.'
    $context.Source = $realSource
    $context.Installed = Join-Path $scratch 'installed'
    New-Item -ItemType Directory -Path $context.Installed | Out-Null

    # Replace only process/installer boundaries. Exercise the real file backup,
    # hash checks, restore, and startup failure handling against disposable files.
    function Get-AppProcesses {
        return @([pscustomobject]@{ Id = 1; Path = Join-Path $context.Installed 'agent-frow.exe'; StartTime = [datetime]'2020-01-01' })
    }
    function Stop-AppProcesses($Processes) {}
    function Start-InstalledApp([string]$Directory) {
        $script:starts++
        return [pscustomobject]@{ Id = $script:starts + 10 }
    }
    function Assert-AppStarted($Process, [string]$Executable) {
        if ($script:scenario -eq 'startup' -and $script:starts -eq 1) { throw 'simulated startup failure' }
    }
    function Invoke-Native([string]$Command, [string[]]$Arguments) {
        foreach ($name in @('agent-frow.exe', 'agent-frow-hook.exe', 'iCUESDK.x64_2019.dll')) {
            $built = Join-Path $context.Release $name
            if (Test-Path -LiteralPath $built) {
                Copy-Item -LiteralPath $built -Destination (Join-Path $context.Installed $name) -Force
                if ($script:scenario -eq 'partial') { throw 'simulated partial install failure' }
            }
        }
        'new version' | Set-Content -LiteralPath (Join-Path $context.Installed 'version')
    }
    foreach ($scenario in @('partial', 'startup', 'success')) {
        $script:scenario = $scenario
        $script:starts = 0
        foreach ($name in @('agent-frow.exe', 'agent-frow-hook.exe', 'iCUESDK.x64_2019.dll', 'version', 'build-info.json')) {
            "old $name" | Set-Content -LiteralPath (Join-Path $context.Installed $name)
        }
        $settings = Join-Path $context.Installed 'settings.json'
        'settings must survive' | Set-Content -LiteralPath $settings
        if ($scenario -eq 'success') {
            Install-TestBuild $context
            Assert-True ((Get-FileHash -LiteralPath (Join-Path $context.Installed 'agent-frow.exe')).Hash -eq $info.sha256.'agent-frow.exe') 'Installed binary differs.'
        } else {
            Assert-Rejected { Install-TestBuild $context } 'previous installed files restored'
            foreach ($name in @('agent-frow.exe', 'agent-frow-hook.exe', 'iCUESDK.x64_2019.dll', 'version', 'build-info.json')) {
                Assert-True ((Get-Content -LiteralPath (Join-Path $context.Installed $name)) -eq "old $name") "Rollback lost $name."
            }
            Assert-True ($script:starts -ge 1) 'Previous app was not restarted.'
        }
        Assert-True ((Get-Content -LiteralPath $settings) -eq 'settings must survive') 'Settings changed.'
        Assert-True (@(Get-ChildItem -LiteralPath $context.Installed -Force -Filter '.update-*').Count -eq 0) 'Update backup leaked.'
    }
    'tampered' | Set-Content -LiteralPath (Join-Path $context.Release 'agent-frow.exe')
    Assert-Rejected { Install-TestBuild $context } 'artifact changed'
    Assert-True ((Get-FileHash -LiteralPath (Join-Path $context.Installed 'agent-frow.exe')).Hash -eq $info.sha256.'agent-frow.exe') 'A rejected build changed the installed app.'
    Write-Output 'Workflow checks passed: paths, native failures, metadata, packaging, partial install, startup rollback, and tampering.'
} finally {
    Remove-Item -LiteralPath $scratch -Recurse -Force
}
