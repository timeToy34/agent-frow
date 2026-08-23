# Keychron Ultra firmware with per-key brightness

Agent F-Row drives a Keychron Ultra's F-row through the keyboard's own
Launcher protocol; no firmware here is needed for that. What it *is* needed
for is brightness: stock Ultra firmware ignores the per-key value byte and
renders every key at the board's brightness, so on stock firmware every lit
key shows at full and a dark key shows **white**. The fix is a seven-line
change to `app/src/rgb/keychron/per_key_rgb.c`, open upstream as
[Keychron/zmk#9](https://github.com/Keychron/zmk/pull/9) by CalcProgrammer1
(the OpenRGB author). This folder builds Keychron's firmware with that change
applied. Once Keychron ships the fix in a Launcher update, none of this is
needed.

**This is your keyboard and your risk.** The steps below were done on a
V3 Ultra 8K ANSI and it works; the other Ultras run the same firmware from
the same repository, but nobody has flashed them with this. Read all of it
first.

## What is built

- `per-key-brightness.patch` — the pull request's diff, unchanged.
- `build.sh` — clones Keychron's ZMK fork (branch `rtl8762g`, the one the
  Ultras live on; `main` is a plain mirror of upstream ZMK), applies the
  patch, and builds in ZMK's own toolchain container. Needs Docker; works
  from WSL. `./build.sh` builds `keychron_v3_ultra_ansi`; pass another shield
  name from `app/boards/shields/` for another Ultra.
- `check_image.py` — verifies the result the way the keyboard's updater does.

The output, `out/<shield>.bin` (~300 KB), is an app image in the same format
as Keychron's own updates: a Realtek header, a SHA-256 over the payload,
unsigned and unencrypted. It replaces only the application partition; the
Realtek boot and radio patches already on the keyboard stay. It is not
committed here: it links Realtek's prebuilt libraries from Keychron's HAL,
whose licence is not stated, and it is a keyboard-specific binary that goes
stale the day Keychron merges the fix.

## Flashing (USB cable, Chrome or Edge)

The keyboard's updater checks CRC32, SHA-256 and an 8-byte model string,
stages the image to a separate bank, and switches to it only after the check
passes — an interrupted flash leaves the running firmware untouched.

1. Keep a copy of the stock image. The Launcher serves it from
   `https://launcher.keychron.com/vapi/v2/firmware/<vid<<16|pid>>` (the V3
   Ultra is `875826224`); download the current version's `_ota.bin`. That is
   your undo, flashed the same way.
2. Plug the keyboard in by cable, open [launcher.keychron.com](https://launcher.keychron.com),
   connect the keyboard, go to Firmware Update.
3. Click the **version text six times**. A file picker appears (this is the
   Launcher's own manual-upload control; it is just not advertised).
4. Choose `out/<shield>.bin`, flash, do not unplug. The keyboard reboots.
5. `agent-frow doctor` shows the firmware string; a self-built image reports
   the build date instead of Keychron's. Or use the app's Preview row: a dark
   key that renders black (not white) is the fix working.

The Launcher will keep offering to "update" to the stock version, which
undoes the fix. Decline it.

An alternative flasher with the same protocol, as a web page and a Python
script, is in [naaraxi/zmk](https://github.com/naaraxi/zmk) (`openrgb/`).

## What can go wrong

- An image that fails the check is refused; nothing changes.
- An image that passes but does not boot — a build against the wrong shield,
  a broken toolchain — leaves the keyboard without USB, and the only ways
  back (Realtek's UART boot mode, SWD) need the case open. That is why the
  patch is kept to the RGB code and nothing else, why the shield must match
  the keyboard, and why `check_image.py` verifies the model string.
- Everything the fix changes is how per-key colours render. Keymaps, Launcher
  settings and Bluetooth pairings are untouched.
