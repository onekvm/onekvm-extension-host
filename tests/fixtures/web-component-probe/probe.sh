#!/bin/sh
set -eu
case "${1:-}" in
  --health) exit 0 ;;
  echo) cat >/dev/null; printf '{"reply":"ok"}\n' ;;
  *) exit 0 ;;
esac
