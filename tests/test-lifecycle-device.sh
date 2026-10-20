#!/bin/sh
# Read the target device document before invoking this explicit integration test.
# Both packages must come from the OE lifecycle-probe recipe, with the second
# built using LIFECYCLE_PROBE_FAIL_HEALTH=1 and a newer PV.
set -eu
target=${1:?usage: test-lifecycle-device.sh root@device good.ipk bad.ipk}
good=${2:?missing healthy package}
bad=${3:?missing unhealthy package}
ssh -o BatchMode=yes "$target" 'test ! -e /etc/onekvm/extensions/lifecycle-probe.json && test ! -e /data/onekvm/apps/lifecycle-probe'
cleanup() { ssh -o BatchMode=yes "$target" 'onekvm-extension purge lifecycle-probe' || true; }
trap cleanup EXIT HUP INT TERM
ssh -o BatchMode=yes "$target" 'onekvm-extension install-local' < "$good"
ssh -o BatchMode=yes "$target" 'set -eu; onekvm-extension health lifecycle-probe; printf "original database\n" > /data/onekvm/app-data/lifecycle-probe/database; chmod 0666 /data/onekvm/app-data/lifecycle-probe/database'
ssh -o BatchMode=yes "$target" 'onekvm-extension contributions lifecycle-probe.state' | python3 -c 'import json,sys; assert len(json.load(sys.stdin)) == 1'
ssh -o BatchMode=yes "$target" 'onekvm-extension install-local' < "$good"
if ssh -o BatchMode=yes "$target" 'onekvm-extension install-local' < "$bad"; then
    echo 'unhealthy upgrade unexpectedly succeeded' >&2
    exit 1
fi
ssh -o BatchMode=yes "$target" 'onekvm-extension status lifecycle-probe' | python3 -c 'import json,sys; s=json.load(sys.stdin); assert s["enabled"] and s["running"] and s["active_version"] == "1.0.0", s'
ssh -o BatchMode=yes "$target" 'set -eu; test "$(cat /data/onekvm/app-data/lifecycle-probe/database)" = "original database"; onekvm-extension health lifecycle-probe'
ssh -o BatchMode=yes "$target" 'onekvm-extension disable lifecycle-probe'
ssh -o BatchMode=yes "$target" 'onekvm-extension contributions lifecycle-probe.state' | python3 -c 'import json,sys; assert json.load(sys.stdin) == []'
ssh -o BatchMode=yes "$target" 'onekvm-extension enable lifecycle-probe'
ssh -o BatchMode=yes "$target" 'onekvm-extension contributions lifecycle-probe.state' | python3 -c 'import json,sys; assert len(json.load(sys.stdin)) == 1'
ssh -o BatchMode=yes "$target" 'set -eu; onekvm-extension remove lifecycle-probe; test -f /data/onekvm/app-data/lifecycle-probe/database; onekvm-extension purge lifecycle-probe; test ! -e /data/onekvm/app-data/lifecycle-probe; test ! -e /etc/onekvm/extensions/lifecycle-probe.json'
trap - EXIT HUP INT TERM
echo 'Lifecycle device acceptance passed: install, reinstall, failed upgrade/data rollback, disable/enable, remove/purge.'
