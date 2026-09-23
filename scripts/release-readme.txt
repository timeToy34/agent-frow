Agent F-Row @VERSION@ - your coding agents on the keyboard's RGB F-row.
https://github.com/timeToy34/agent-frow

1. Unzip anywhere and run agent-frow.exe once. It installs itself to
   %LOCALAPPDATA%\agent-frow, registers its hooks with every agent it
   finds - and, for Claude, its status line, wrapping the one you have so
   it renders exactly as before - and continues from the installed copy;
   this folder can then be deleted. Upgrading is the same gesture with a
   newer zip.
2. For first-time setup or hook configuration changes, restart your agents.
   For Codex, run /hooks and trust any Agent F-Row entry marked for review.

Devices (all optional; the app runs fine without any, the window shows
everything):
- Corsair, the F-row remapped to F13-F24: iCUE running, the remap in the
  DEFAULT profile - a profile switch takes the summon keys with it.
- Keychron Ultra, the remap in the Launcher keymap; light over the cable or
  the 2.4 GHz receiver, not Bluetooth. Per-key brightness needs Keychron's
  firmware fix (Keychron/zmk pull request 9).
- Keychron V0 Ultra numpad: export your current Launcher keymap as a backup,
  then import the included keychron_v0_ultra_ansi.json over the cable.
  Upgrading from 0.8.0 or earlier requires the new map: the top row sends
  single keys (Intl1, Intl5, Intl6, Keypad Comma); Ctrl+Shift+F21-F24 is no
  longer captured. The map is unchanged from 0.8.1.
  The knob and M1-M5 retain Ctrl+Shift+F13-F20. One agent per M key; the top
  line shows the one the knob picks. The new map was tested on US Windows.
  Importing the map needs no firmware flash; lighting still needs the
  existing per-key-brightness firmware fix built for the V0.
- Stream Deck: quit the Stream Deck app. One row per lane - name, numbers,
  state; every key summons, and while a lane waits the middle keys answer.
- The monitor: the Mini mode button, or a double-click on a lane, folds the
  window to one row per agent. Drag it anywhere, resize it by its corner,
  double-click to come back; it reopens where you left it.

Windows may warn on first run (SmartScreen): the zip is not code-signed.

License: MIT, see LICENSE.txt. iCUESDK.x64_2019.dll is Corsair's, covered by
Corsair's iCUE SDK EULA rather than the MIT license:
https://corsairofficial.github.io/cue-sdk/#end-user-license-agreement
