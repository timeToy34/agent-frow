#!/usr/bin/env bash
# Builds Keychron's ZMK firmware for a Keychron Ultra with the per-key
# brightness fix applied, in ZMK's own toolchain container. Needs docker.
#
#   ./build.sh                            # keychron_v3_ultra_ansi
#   ./build.sh keychron_q3_ultra_ansi     # another Ultra shield
#
# Output: out/<shield>.bin, checked by check_image.py. Nothing is flashed.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SHIELD="${1:-keychron_v3_ultra_ansi}"
BRANCH=rtl8762g
IMAGE=zmkfirmware/zmk-build-arm:3.5
WS="$HERE/zmk"

command -v docker >/dev/null || { echo "docker is needed" >&2; exit 1; }
if [ ! -d "$WS/app" ]; then
  echo ">> cloning Keychron/zmk ($BRANCH)"
  git clone --depth 1 -b "$BRANCH" https://github.com/Keychron/zmk "$WS"
fi
cd "$WS"
if git apply --reverse --check "$HERE/per-key-brightness.patch" >/dev/null 2>&1; then
  echo ">> patch already applied"
else
  git apply "$HERE/per-key-brightness.patch" && echo ">> patch applied"
fi
CONF="app/boards/shields/$SHIELD/$SHIELD.conf"
[ -f "$CONF" ] || { echo "no shield $SHIELD (see app/boards/shields/)" >&2; exit 1; }
MODEL="$(sed -n 's/^CONFIG_KEYCHRON_FWU_STRING_NAME="\(.*\)"/\1/p' "$CONF")"

docker image inspect "$IMAGE" >/dev/null 2>&1 || docker pull "$IMAGE"
echo ">> building $SHIELD (first run initialises the west workspace: several minutes)"
# The build directory must be app/build: Keychron's post-build header step
# hardcodes that path.
docker run --rm -u "$(id -u):$(id -g)" -e HOME=/tmp \
  -v "$WS":/workspaces/zmk -w /workspaces/zmk "$IMAGE" bash -lc "
    set -e
    if [ ! -d zephyr ] || [ ! -d modules ]; then
      west init -l app && west update && west zephyr-export
    fi
    west build -s app -p -b keychron -d app/build -- -DSHIELD=$SHIELD
  "
mkdir -p "$HERE/out"
cp app/build/zephyr/zmk.bin "$HERE/out/$SHIELD.bin"
echo ">> out/$SHIELD.bin"
python3 "$HERE/check_image.py" "$HERE/out/$SHIELD.bin" "$MODEL"
sha256sum "$HERE/out/$SHIELD.bin"
