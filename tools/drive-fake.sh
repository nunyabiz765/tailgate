#!/bin/bash
# Clicks through every mutating control against tests/fake-tailscale on a private
# Xvfb display, saving a screenshot after each step. Prints the recorded argv.
cd "$(dirname "$0")/.."
S=${1:-/tmp/tg-shots}; mkdir -p "$S"
T=/tmp/tg-fake; rm -rf $T; mkdir -p $T/cfg
export FAKE_TS_LOG=$T/ts.log FAKE_TS_STATE=$T/state FAKE_PKEXEC_LOG=$T/pkexec.log
export TAILGATE_TAILSCALE_BIN=$PWD/tests/fake-tailscale TAILGATE_PKEXEC_BIN=$PWD/tests/fake-pkexec
export TAILGATE_OPEN_BIN=$T/fake-open TAILGATE_CONFIG_DIR=$T/cfg TAILGATE_IPCHECK_URL=file://$T/ip.json
export FAKE_TS_MODE=denied-until-operator
printf '#!/bin/sh\necho "$@" >> %s/open.log\n' $T > $T/fake-open; chmod +x $T/fake-open
echo '{"ip":"198.51.100.7","country":"Sweden","city":"Stockholm","mullvad_exit_ip":true,"mullvad_exit_ip_hostname":"se-sto-wg-001"}' > $T/ip.json
N=78; tools/xvfb-run-app.sh $N 1000x780; sleep 4
x() { tools/xclick.py :$N "$@"; }
shot() { sleep "${2:-1.2}"; DISPLAY=:$N import -window root "$S/$1.png"; }
mark() { echo "--- $1"; }
shot f01-start 0
# Tirana row, the permission flow on an exit-node switch
mark "switch while denied"; x click 620 447; shot f02-permission
mark "grant operator"; x click 500 "342"; shot f03-after-grant 2
mark "favorite Tirana"; x click 931 "447"; shot f04-favorite
mark "search"; x click 650 142; x type zurich; shot f05-search
x key Escape; sleep .3
mark "every server"; x click 943 185; shot f06-every; x click 943 185; sleep .4
mark "lan off"; x click 313 454; shot f07-lan 1.5
mark "check ip"; x click 282 668; shot f08-ip 1.5
mark "exit off prompt"; x click 313 256; shot f09-exit-confirm
mark "exit off cancel by Esc"; x key Escape; shot f09b-escaped
mark "exit off prompt again"; x click 313 256; sleep 1
mark "exit off go"; x click 635 437; shot f10-exit-off 2
mark "exit back on"; x click 313 256; shot f11-exit-on 2
mark "down prompt"; x click 296 ${DOWN_Y:-529}; shot f12-down-confirm
mark "down go"; x click 638 509; shot f13-down 2
mark "up"; x click 296 ${DOWN_Y:-529}; shot f14-up-again 2
mark "devices"; x click 531 92; shot f15-devices
mark "admin link"; x click 186 742; sleep .5
mark "about"; x click 307 85; shot f16-about
x key Escape; sleep .3
tools/xvfb-stop.sh $N
