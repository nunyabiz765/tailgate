#!/bin/bash
# The one-click install flow against fakes, on a private Xvfb display:
# tools/drive-install.sh [SHOTDIR]. Needs Xvfb, ImageMagick (import), python3-xlib.
cd "$(dirname "$0")/.."
S=${1:-/tmp/tg-ez-shots}; mkdir -p "$S"
T=/tmp/tg-ez; rm -rf $T; mkdir -p $T
printf '#!/bin/sh\necho "$@" >> %s/open.log\n' $T > $T/fake-open; chmod +x $T/fake-open
N=${DISPLAYNUM:-78}
run() { # name size extra-env...
  local size=$2; shift 2
  rm -rf $T/cfg $T/state $T/ts.log $T/tailscale; mkdir -p $T/cfg
  env FAKE_TS_LOG=$T/ts.log FAKE_TS_STATE=$T/state FAKE_PKEXEC_LOG=$T/pkexec.log FAKE_TS_MODE=login \
    FAKE_INSTALL_TARGET=$T/tailscale FAKE_INSTALL_DELAY=1.5 \
    TAILGATE_TAILSCALE_BIN=$T/tailscale TAILGATE_PKEXEC_BIN=$PWD/tests/fake-pkexec \
    TAILGATE_INSTALL_HELPER=$PWD/tests/fake-install-tailscale \
    TAILGATE_OPEN_BIN=$T/fake-open TAILGATE_CONFIG_DIR=$T/cfg "$@" tools/xvfb-run-app.sh $N $size
  sleep 3.5
}
stop() { tools/xvfb-stop.sh $N; sleep .4; }
x() { tools/xclick.py :$N "$@"; }
shot() { sleep "${2:-1}"; DISPLAY=:$N import -window root "$S/$1.png"; }

# Not installed, .deb helper present: one Install button, its progress, then sign-in.
rm -f $T/pkexec.log
run ez-install 1000x780
shot ez-1-install 0
x click 503 287; shot ez-2-progress 2.6
shot ez-3-signin 9
echo "pkexec: $(cat $T/pkexec.log)"
echo "operator: $(grep -o '"operator": "[^"]*"' $T/state)"
x click 513 350; sleep 1.5; echo "open.log: $(cat $T/open.log 2>/dev/null)"
stop
fails=0
[ "$(cat $T/pkexec.log)" = "$PWD/tests/fake-install-tailscale" ] || { echo "FAIL: helper not run once through pkexec"; fails=1; }
grep -q "\"operator\": \"$(id -un)\"" $T/state || { echo "FAIL: operator not set"; fails=1; }
grep -q '^https://login.tailscale.com/' $T/open.log || { echo "FAIL: sign-in didn't open the login page"; fails=1; }

# Offline: the error stays on screen and the button is ready again.
run ez-offline 1000x780 FAKE_INSTALL_MODE=offline
x click 503 287; shot ez-4-offline 5
stop

# Installed but tailscaled off: Start Tailscale, same helper.
run ez-start 1000x780 FAKE_TS_MODE=fail TAILGATE_TAILSCALE_BIN=$PWD/tests/fake-tailscale
shot ez-5-start 0
stop

# The tarball has no helper: Terminal steps, as before.
run ez-nohelper 1000x780 TAILGATE_INSTALL_HELPER=/nonexistent
shot ez-6-no-helper 0
stop
exit $fails
