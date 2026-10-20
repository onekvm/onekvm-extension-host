#!/bin/sh
set -eu
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
workspace=$(dirname "$repo")
fixture=$repo/tests/fixtures/lifecycle-probe
"${BUN:-$HOME/.bun/bin/bun}" "$workspace/tmp/pocketjs/tools/build.ts" "$fixture/main.tsx" \
    --framework=vue-vapor --project-root="$fixture" --outdir="$fixture/screen-app" \
    --font-regular="$workspace/onekvm-screen-display/.fonts/NotoSansCJK-SC-Regular.otf"
python3 - "$fixture/screen-app/icon.bin" <<'PY'
import pathlib,sys
pathlib.Path(sys.argv[1]).write_bytes(b'ICO1\x01\x00\x01\x00\x00\xf8\xff')
PY
