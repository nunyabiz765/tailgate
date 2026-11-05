#!/bin/bash
# Failure and layout states against the fake, each on a private Xvfb display.
cd "$(dirname "$0")/.."
S=${1:-/tmp/tg-shots}; mkdir -p "$S"
T=/tmp/tg-fake2; rm -rf $T; mkdir -p $T
printf '#!/bin/sh\necho "$@" >> %s/open.log\n' $T > $T/fake-open; chmod +x $T/fake-open
run() { # name mode size extra-env...
  local name=$1 mode=$2 size=$3; shift 3
  rm -rf $T/cfg $T/state $T/ts.log; mkdir -p $T/cfg
  env FAKE_TS_LOG=$T/ts.log FAKE_TS_STATE=$T/state FAKE_PKEXEC_LOG=$T/pkexec.log FAKE_TS_MODE=$mode \
    TAILGATE_TAILSCALE_BIN=$PWD/tests/fake-tailscale TAILGATE_PKEXEC_BIN=$PWD/tests/fake-pkexec \
    TAILGATE_OPEN_BIN=$T/fake-open TAILGATE_CONFIG_DIR=$T/cfg "$@" tools/xvfb-run-app.sh 79 $size
  sleep 3.5
}
stop() { tools/xvfb-stop.sh 79; sleep .4; }
x() { tools/xclick.py :79 "$@"; }
shot() { sleep "${2:-1}"; DISPLAY=:79 import -window root "$S/$1.png"; }

# Slow CLI: reads hang, window must stay responsive and say so.
run s-slow slow 1000x780 TAILGATE_READ_TIMEOUT_MS=1200
shot s-slow-a 0
x click 531 92; shot s-slow-tab .6   # tab change while the poller is stuck
stop

# Node gone between listing and switching: Tirana row, error stays on screen
run s-gone gone 1000x780
x click 620 447; shot s-gone 1.5
stop

# Missing CLI
run s-missing ok 1000x780 TAILGATE_TAILSCALE_BIN=/nonexistent/tailscale
shot s-missing 0
stop

# Malformed output
run s-malformed malformed 1000x780
shot s-malformed 0
stop

# Daemon not running
run s-fail fail 1000x780
shot s-fail 0
stop

# Logged out: Log in opens the AuthURL
run s-login login 1000x780
shot s-login 0
x click 296 520; sleep .6; echo "open.log: $(cat $T/open.log)"
stop

# Stopped from the start, and the node list isn't known yet
run s-stopped stopped 1000x780
shot s-stopped 0
stop

# Just this once: stays denied, one pkexec'd change, no operator
rm -f $T/pkexec.log
run s-once denied 1000x780
x click 313 454; shot s-once-a 1.2
x click 500 458; shot s-once-b 1.5
echo "once pkexec: $(cat $T/pkexec.log)"; echo "once set calls: $(grep -c '"set"' $T/ts.log)"
stop

# Node vanished from under the favorites star
run s-dark ok 1000x780
stop
mkdir -p $T/cfg; cat > $T/cfg/state.json <<JSON
{"favorites":[{"id":"gone-node","label":"Atlantis"}],"dark":true,"last_used":{"id":"gone-node","label":"Atlantis"},"all_servers":false}
JSON
env FAKE_TS_LOG=$T/ts.log FAKE_TS_STATE=$T/state FAKE_TS_MODE=ok TAILGATE_TAILSCALE_BIN=$PWD/tests/fake-tailscale TAILGATE_CONFIG_DIR=$T/cfg tools/xvfb-run-app.sh 79 1000x780
sleep 3.5; shot s-dark 0
x click 931 271; sleep .3
stop

# Small window, light and dark
run s-small ok 420x640; x resize 420 640; shot s-small-status 1
x click 130 195; shot s-small-nodes 1
stop
