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
   finds - and, for Claude, its status line, wrapping the one you have so
   it renders exactly as before - and continues from the installed copy;
   this folder can then be deleted. Upgrading is the same gesture with a
   newer zip.
2. Restart your agents. For Codex, also run /hooks inside it and trust the
   entry, or its hooks will never run.

Devices (all optional; the app runs fine without any, the window shows
everything):
- Corsair, the F-row remapped to F13-F24: iCUE running, the remap in the
  DEFAULT profile - a profile switch takes the summon keys with it.
- Keychron Ultra, the remap in the Launcher keymap; light over the cable or
  the 2.4 GHz receiver, not Bluetooth. Per-key brightness needs Keychron's
  firmware fix (Keychron/zmk pull request 9).
- Keychron V0 Ultra numpad: import the keymap file from the repository
  (firmware/keychron-ultra/keymaps) in the Launcher's Keymap tab - the knob
  and nine keys send Ctrl+Shift+F13-F24. One agent per M key, the top line
  shows the one the knob picks; same cable-or-receiver rule and the same
  firmware fix, built for the V0.
- Stream Deck: quit the Stream Deck app. One row per lane - name, numbers,
  state; every key summons, and while a lane waits the middle keys answer.
- The monitor: the Mini mode button, or a double-click on a lane, folds the
  window to one row per agent. Drag it anywhere, resize it by its corner,
  double-click to come back; it reopens where you left it.

Windows may warn on first run (SmartScreen): the zip is not code-signed.

License: MIT, see LICENSE.txt. iCUESDK.x64_2019.dll is Corsair's, covered by
Corsair's iCUE SDK EULA rather than the MIT license:
https://corsairofficial.github.io/cue-sdk/#end-user-license-agreement
"@ | Set-Content -Encoding UTF8 (Join-Path $stage 'README.txt')

New-Item -ItemType Directory (Join-Path $root 'dist') -Force | Out-Null
$zip = Join-Path $root 'dist\agent-frow-win64.zip'
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zip -Force
Get-Item $zip | Select-Object FullName, Length
