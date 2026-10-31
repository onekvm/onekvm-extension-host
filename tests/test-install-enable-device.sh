#!/bin/sh
# Explicit device acceptance; read its device document before invoking.
set -eu
target=${1:?usage: test-install-enable-device.sh root@device probe.ipk}
package=${2:?missing OE lifecycle-probe IPK}
ssh -o BatchMode=yes "$target" 'test ! -e /etc/onekvm/extensions/lifecycle-probe.json && test ! -e /data/onekvm/apps/lifecycle-probe'
cleanup() { ssh -o BatchMode=yes "$target" 'onekvm-extension purge lifecycle-probe' || true; }
trap cleanup EXIT HUP INT TERM
check_enabled() {
    ssh -o BatchMode=yes "$target" 'onekvm-extension status lifecycle-probe' |
        python3 -c 'import json,sys; s=json.load(sys.stdin); expected=sys.argv[1] == "true"; assert s["enabled"] == expected and s["running"] == expected, s' "$1"
}
ssh -o BatchMode=yes "$target" 'onekvm-extension install-local --enable-after-install=false' < "$package"
check_enabled false
# An update retains the previous explicit disable, even with the flag enabled.
ssh -o BatchMode=yes "$target" 'onekvm-extension install-local --enable-after-install=true' < "$package"
check_enabled false
ssh -o BatchMode=yes "$target" 'onekvm-extension remove lifecycle-probe'
# A fresh install after removal honors the choice despite the retained state.
ssh -o BatchMode=yes "$target" 'onekvm-extension install-local --enable-after-install=true' < "$package"
check_enabled true
# Reinstalling an enabled plugin does not disable it.
ssh -o BatchMode=yes "$target" 'onekvm-extension install-local --enable-after-install=false' < "$package"
check_enabled true
cleanup
trap - EXIT HUP INT TERM
echo 'Install enable choice passed: disabled install, preserved upgrades, and reinstall after removal.'
