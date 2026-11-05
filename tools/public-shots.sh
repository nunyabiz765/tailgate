#!/bin/bash
# Retakes docs/screenshots from the fake CLI and its synthetic data, on a private
# Xvfb display. Runs the fake from a neutral path, since the banner names it, and
# sets no operator, so the window shows neither a home directory nor a username.
cd "$(dirname "$0")/.."
OUT=docs/screenshots; mkdir -p "$OUT"
T=/tmp/tailgate-demo; rm -rf $T; mkdir -p $T/cfg
ln -s "$PWD/tests/fake-tailscale" $T/tailscale
printf '#!/bin/sh\n' > $T/open; chmod +x $T/open
echo '{"ip":"198.51.100.7","country":"Sweden","city":"Stockholm","mullvad_exit_ip":true,"mullvad_exit_ip_hostname":"se-sto-wg-001"}' > $T/ip.json
export FAKE_TS_LOG=$T/ts.log FAKE_TS_STATE=$T/state FAKE_TS_MODE=ok FAKE_TS_FIXTURE=$PWD/tests/fixtures/status.json
export TAILGATE_TAILSCALE_BIN=$T/tailscale TAILGATE_OPEN_BIN=$T/open TAILGATE_CONFIG_DIR=$T/cfg TAILGATE_IPCHECK_URL=file://$T/ip.json
N=77; tools/xvfb-run-app.sh $N 1000x780; sleep 4
x() { tools/xclick.py :$N "$@"; }
shot() { sleep "${2:-1.2}"; DISPLAY=:$N import -window root "$OUT/$1.png"; }
shot exit-nodes 0
x click 650 142; x type zurich; shot search; x key Escape; sleep .3
x click 282 622; sleep 1.5; shot check-ip 0
x click 300 529; shot turn-off-dialog; x key Escape; sleep .4
x click 531 92; shot devices
x click 622 92; shot serving
x click 307 85; shot about; x key Escape; sleep .4
x click 435 92; sleep .3; x click 351 85; shot dark
x click 351 85
tools/xvfb-stop.sh $N
