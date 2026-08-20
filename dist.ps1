# Builds dist\agent-frow-win64.zip: a fresh release build, the iCUE SDK DLL if
# the build found one, the MIT license, and a README with the install steps.
# Run from anywhere; everything is relative to this script.
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $MyInvocation.MyCommand.Path
Set-Location $root

# cargo writes progress to stderr, which Stop-preference PowerShell would
# treat as a terminating error; let cmd merge the streams first.
cmd /c "cargo build --release 2>&1"
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$stage = Join-Path $root 'target\dist-stage'
Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory $stage | Out-Null

Copy-Item (Join-Path $root 'target\release\agent-frow.exe') $stage
Copy-Item (Join-Path $root 'target\release\agent-frow-hook.exe') $stage
$dll = Join-Path $root 'target\release\iCUESDK.x64_2019.dll'
if (Test-Path $dll) {
    Copy-Item $dll $stage
} else {
    Write-Warning 'no iCUE SDK DLL in the build; the zip will ship without lighting'
}
# MIT's notice must travel with every copy, binaries included.
Copy-Item (Join-Path $root 'LICENSE') (Join-Path $stage 'LICENSE.txt')

@"
Agent F-Row - your coding agents on the keyboard's RGB F-row.
https://github.com/timeToy34/agent-frow

1. Unzip anywhere and run agent-frow.exe once. It installs itself to
   %LOCALAPPDATA%\agent-frow, registers its hooks with every agent it
   finds, and continues from the installed copy; this folder can then be
   deleted. Upgrading is the same gesture with a newer zip.
2. Restart your agents. For Codex, also run /hooks inside it and trust the
   entry, or its hooks will never run.

Keyboard (optional): iCUE with a Corsair board, and the F-row remapped to
F13-F24 in the DEFAULT profile - a profile switch takes the summon keys with
it. The app runs fine without any of this; the window shows everything.

Windows may warn on first run (SmartScreen): the zip is not code-signed.

License: MIT, see LICENSE.txt. iCUESDK.x64_2019.dll is Corsair's, covered by
Corsair's iCUE SDK EULA rather than the MIT license:
https://corsairofficial.github.io/cue-sdk/#end-user-license-agreement
"@ | Set-Content -Encoding UTF8 (Join-Path $stage 'README.txt')

New-Item -ItemType Directory (Join-Path $root 'dist') -Force | Out-Null
$zip = Join-Path $root 'dist\agent-frow-win64.zip'
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip -Force
Get-Item $zip | Select-Object FullName, Length
