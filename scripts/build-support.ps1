# Shared by the developer entry point, packaging, and the workflow checks.
Set-StrictMode -Version Latest

function Invoke-Native([string]$Command, [string[]]$Arguments) {
    $preference = $ErrorActionPreference
    try {
        # Windows PowerShell treats even Cargo's progress on stderr as errors.
        $ErrorActionPreference = 'Continue'
        & $Command @Arguments 2>&1 | ForEach-Object { Write-Host $_.ToString() }
        $code = $LASTEXITCODE
    } finally { $ErrorActionPreference = $preference }
    if ($code -ne 0) { throw "$Command failed with exit code $code" }
}

function Get-SourceGit([string]$Source, [string[]]$Arguments) {
    $safe = $Source.Replace('\', '/')
    if ($safe.StartsWith('//')) { $safe = '%(prefix)/' + $safe }
    $preference = $ErrorActionPreference
    try {
        $ErrorActionPreference = 'Continue'
        $result = & git -c "safe.directory=$safe" -C $Source @Arguments 2>&1
        $code = $LASTEXITCODE
    } finally { $ErrorActionPreference = $preference }
    if ($code -ne 0) { throw "git failed: $($result -join [Environment]::NewLine)" }
    return $result
}

function Test-Within([string]$Path, [string]$Parent) {
    return $Path.Equals($Parent, [StringComparison]::OrdinalIgnoreCase) -or
        $Path.StartsWith($Parent.TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase)
}

function New-BuildContext([string]$SourceRoot, [string]$OutputRoot) {
    if ($env:OS -ne 'Windows_NT') { throw 'Run this script with Windows PowerShell and Windows Cargo.' }
    if ($OutputRoot -notmatch '^[A-Za-z]:[\\/]') {
        throw 'OutputRoot must be an absolute path on a local Windows drive.'
    }
    $source = [IO.Path]::GetFullPath($SourceRoot).TrimEnd('\')
    $output = [IO.Path]::GetFullPath($OutputRoot).TrimEnd('\')
    $installed = Join-Path $env:LOCALAPPDATA 'agent-frow'
    if ($output.Length -le 2 -or (Test-Within $output $source) -or
        (Test-Within $source $output) -or (Test-Within $output $installed) -or
        (Test-Within $installed $output)) {
        throw 'OutputRoot must be separate from the source checkout and installed app, and cannot be a drive root.'
    }
    if (Test-Path -LiteralPath (Join-Path $output '.git')) {
        throw 'OutputRoot is still a Git checkout. Retire that duplicate checkout before using it for build outputs.'
    }
    foreach ($command in @('cargo', 'rustc', 'git')) { Get-Command $command -ErrorAction Stop | Out-Null }
    $rust = & rustc -vV
    if ($LASTEXITCODE -ne 0 -or $rust -notcontains 'host: x86_64-pc-windows-msvc') {
        throw 'The build requires the native x86_64-pc-windows-msvc Rust toolchain.'
    }
    if ($env:CARGO_BUILD_TARGET) { throw 'Unset CARGO_BUILD_TARGET; this workflow uses the native Windows target.' }
    $marker = Join-Path $output 'build-source.json'
    if (Test-Path -LiteralPath $marker) {
        $owner = Get-Content -LiteralPath $marker -Raw | ConvertFrom-Json
        if (-not $source.Equals($owner.source, [StringComparison]::OrdinalIgnoreCase)) {
            throw "This output folder belongs to $($owner.source), not $source."
        }
    }
    $manifest = Get-Content -LiteralPath (Join-Path $source 'Cargo.toml') -Raw
    $versionMatch = [regex]::Match($manifest, '(?m)^version = "([^"]+)"\r?$')
    if (-not $versionMatch.Success) { throw 'Cannot read the workspace version.' }
    Get-SourceGit $source @('rev-parse', '--show-toplevel') | Out-Null
    New-Item -ItemType Directory -Path $output -Force | Out-Null
    @{ source = $source } | ConvertTo-Json | Set-Content -LiteralPath $marker -Encoding UTF8
    return [pscustomobject]@{
        Source = $source; Output = $output; Installed = $installed
        Target = Join-Path $output 'target'
        Release = Join-Path $output 'target\release'
        Dist = Join-Path $output 'dist'
        Version = $versionMatch.Groups[1].Value
    }
}

function Write-BuildMetadata($Context) {
    $files = @('agent-frow.exe', 'agent-frow-hook.exe')
    $sdk = Join-Path $Context.Source 'iCUESDK\redist\x64\iCUESDK.x64_2019.dll'
    $builtSdk = Join-Path $Context.Release 'iCUESDK.x64_2019.dll'
    if (Test-Path -LiteralPath $sdk) {
        Copy-Item -LiteralPath $sdk -Destination $builtSdk -Force
        $files += 'iCUESDK.x64_2019.dll'
    } elseif (Test-Path -LiteralPath $builtSdk) { Remove-Item -LiteralPath $builtSdk }
    $hashes = [ordered]@{}
    foreach ($name in $files) {
        $hashes[$name] = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $Context.Release $name)).Hash.ToLowerInvariant()
    }
    $dirty = @(Get-SourceGit $Context.Source @('status', '--porcelain')).Count -gt 0
    [ordered]@{
        version = $Context.Version
        source = $Context.Source
        commit = [string](Get-SourceGit $Context.Source @('rev-parse', 'HEAD'))
        dirty = $dirty
        built_at = [DateTime]::UtcNow.ToString('o')
        sha256 = $hashes
    } | ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $Context.Release 'build-info.json') -Encoding UTF8
}

function Get-AppProcesses {
    return @(Get-Process -Name 'agent-frow' -ErrorAction SilentlyContinue | Select-Object Id, Path, StartTime)
}

function Stop-AppProcesses($Processes) {
    foreach ($old in $Processes) {
        $current = Get-Process -Id $old.Id -ErrorAction SilentlyContinue
        if ($null -ne $current -and $current.StartTime -eq $old.StartTime) {
            Stop-Process -Id $old.Id -Force -ErrorAction Stop
            if (-not $current.WaitForExit(5000)) { throw "App process $($old.Id) did not exit." }
        }
    }
}

function Start-InstalledApp([string]$Directory) {
    return Start-Process -FilePath (Join-Path $Directory 'agent-frow.exe') -WorkingDirectory $Directory -WindowStyle Hidden -PassThru
}

function Assert-AppStarted($Process, [string]$Executable) {
    $deadline = [DateTime]::UtcNow.AddSeconds(10)
    do {
        Start-Sleep -Milliseconds 200
        $Process.Refresh()
        if ($Process.HasExited) { throw 'The installed app exited during startup.' }
        $listener = @(Get-NetTCPConnection -LocalPort 47115 -State Listen -ErrorAction SilentlyContinue |
            Where-Object { $_.OwningProcess -eq $Process.Id })
        if ($listener.Count -gt 0) {
            $actual = Get-Process -Id $Process.Id -ErrorAction Stop
            if ($actual.Path -ne $Executable) { throw 'The process is running a different executable.' }
            return
        }
    } while ([DateTime]::UtcNow -lt $deadline)
    throw 'The installed app did not open its ingress port within 10 seconds.'
}

function Install-TestBuild($Context) {
    $executable = Join-Path $Context.Installed 'agent-frow.exe'
    $oldProcesses = @(Get-AppProcesses)
    foreach ($process in $oldProcesses) {
        if ($process.Path -ne $executable) {
            throw "An app is running outside the installed location: $($process.Path). Quit it first."
        }
    }
    $metadataPath = Join-Path $Context.Release 'build-info.json'
    $metadata = Get-Content -LiteralPath $metadataPath -Raw | ConvertFrom-Json
    foreach ($file in $metadata.sha256.PSObject.Properties) {
        if ((Get-FileHash -LiteralPath (Join-Path $Context.Release $file.Name)).Hash -ne $file.Value) {
            throw "Build artifact changed after compilation: $($file.Name)"
        }
    }
    $names = @('agent-frow.exe', 'agent-frow-hook.exe', 'iCUESDK.x64_2019.dll', 'version', 'build-info.json')
    $backup = Join-Path $Context.Installed ('.update-' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $backup -Force | Out-Null
    foreach ($name in $names) {
        $path = Join-Path $Context.Installed $name
        if (Test-Path -LiteralPath $path) { Copy-Item -LiteralPath $path -Destination $backup }
    }
    $launched = $null
    $keepBackup = $false
    try {
        Invoke-Native (Join-Path $Context.Release 'agent-frow.exe') @('install')
        Stop-AppProcesses $oldProcesses
        foreach ($file in $metadata.sha256.PSObject.Properties) {
            if ((Get-FileHash -LiteralPath (Join-Path $Context.Installed $file.Name)).Hash -ne $file.Value) {
                throw "Installed artifact does not match the build: $($file.Name)"
            }
        }
        Copy-Item -LiteralPath $metadataPath -Destination (Join-Path $Context.Installed 'build-info.json') -Force
        $launched = Start-InstalledApp $Context.Installed
        Assert-AppStarted $launched $executable
        foreach ($name in @('version', 'build-info.json')) {
            $previous = Join-Path $backup $name
            $aside = Join-Path $Context.Installed "$name.old"
            if (Test-Path -LiteralPath $previous) { Copy-Item -LiteralPath $previous -Destination $aside -Force }
            elseif (Test-Path -LiteralPath $aside) { Remove-Item -LiteralPath $aside }
        }
        Write-Output "Installed build verified and running: $executable (PID $($launched.Id))"
    } catch {
        $failure = $_
        $keepBackup = $true
        if ($null -ne $launched) { Stop-AppProcesses @($launched) }
        Stop-AppProcesses $oldProcesses
        foreach ($name in $names) {
            $saved = Join-Path $backup $name
            $destination = Join-Path $Context.Installed $name
            if (Test-Path -LiteralPath $saved) { Copy-Item -LiteralPath $saved -Destination $destination -Force }
            elseif (Test-Path -LiteralPath $destination) { Remove-Item -LiteralPath $destination }
        }
        if ($oldProcesses.Count -gt 0) {
            $restored = Start-InstalledApp $Context.Installed
            Assert-AppStarted $restored $executable
        }
        $keepBackup = $false
        throw "Update failed; previous installed files restored. $failure"
    } finally {
        if (-not $keepBackup) { Remove-Item -LiteralPath $backup -Recurse -Force }
        else { Write-Warning "Recovery files retained at $backup" }
    }
}

function Assert-NewPackage($Context) {
    $zip = Join-Path $Context.Dist "agent-frow-$($Context.Version)-win64.zip"
    if ((Test-Path -LiteralPath $zip) -or (Test-Path -LiteralPath "$zip.sha256")) {
        throw "Release already exists: $zip. Existing releases are never overwritten."
    }
}

function Publish-Package($Context) {
    Assert-NewPackage $Context
    $stage = Join-Path $Context.Target ('package-' + [guid]::NewGuid().ToString('N'))
    $payload = Join-Path $stage 'payload'
    $zipName = "agent-frow-$($Context.Version)-win64.zip"
    $published = $false
    New-Item -ItemType Directory -Path $payload -Force | Out-Null
    try {
        foreach ($name in @('agent-frow.exe', 'agent-frow-hook.exe', 'build-info.json')) {
            Copy-Item -LiteralPath (Join-Path $Context.Release $name) -Destination $payload
        }
        $sdk = Join-Path $Context.Release 'iCUESDK.x64_2019.dll'
        if (Test-Path -LiteralPath $sdk) { Copy-Item -LiteralPath $sdk -Destination $payload }
        else { Write-Warning 'Packaging without Corsair lighting: no iCUE SDK DLL.' }
        Copy-Item -LiteralPath (Join-Path $Context.Source 'LICENSE') -Destination (Join-Path $payload 'LICENSE.txt')
        Copy-Item -LiteralPath (Join-Path $Context.Source 'firmware\keychron-ultra\keymaps\keychron_v0_ultra_ansi.json') -Destination $payload
        $readme = Get-Content -LiteralPath (Join-Path $Context.Source 'scripts\release-readme.txt') -Raw
        $readme.Replace('@VERSION@', $Context.Version) | Set-Content -LiteralPath (Join-Path $payload 'README.txt') -Encoding UTF8
        $temporary = Join-Path $stage $zipName
        Compress-Archive -Path (Join-Path $payload '*') -DestinationPath $temporary
        $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $temporary).Hash.ToLowerInvariant()
        "$hash  $zipName" | Set-Content -LiteralPath "$temporary.sha256" -Encoding ASCII
        New-Item -ItemType Directory -Path $Context.Dist -Force | Out-Null
        Move-Item -LiteralPath $temporary -Destination (Join-Path $Context.Dist $zipName) -ErrorAction Stop
        $published = $true
        Move-Item -LiteralPath "$temporary.sha256" -Destination (Join-Path $Context.Dist "$zipName.sha256") -ErrorAction Stop
        Write-Output "Release package: $(Join-Path $Context.Dist $zipName)"
    } catch {
        if ($published) { Remove-Item -LiteralPath (Join-Path $Context.Dist $zipName) -Force }
        throw
    } finally { Remove-Item -LiteralPath $stage -Recurse -Force }
}
