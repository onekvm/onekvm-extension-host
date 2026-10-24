#!/bin/sh
# Construct/decode real sd-bus messages on a private bus; no unit is started.
set -eu
repo=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
probe=$(mktemp -d)
trap 'rm -rf "$probe"' EXIT HUP INT TERM
${CC:-cc} -ffunction-sections -fdata-sections -Wl,--gc-sections \
    "$repo/tests/transient-properties.c" $(pkg-config --cflags --libs libsystemd) \
    -o "$probe/transient-properties"
dbus-run-session -- "$probe/transient-properties"
