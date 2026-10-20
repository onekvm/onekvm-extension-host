#!/bin/sh
set -eu
case "${1:-}" in
  --health)
    if [ '@FAIL_HEALTH@' = 1 ]; then
      printf 'incompatible migration\n' > "$ONEKVM_EXTENSION_DATA/database"
      exit 1
    fi
    exit 0
    ;;
  --status) printf '{"version":"@VERSION@"}\n'; exit 0 ;;
esac
while :; do sleep 60; done
