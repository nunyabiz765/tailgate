#!/bin/bash
# Screenshots of the Shares tab from stand-ins only: the fake tailscale CLI and its
# synthetic tailnet, and tests/fake-smb for every SMB tool, on a private Xvfb display.
# Nothing is probed or mounted. usage: tools/shares-shots.sh [OUTDIR]
cd "$(dirname "$0")/.."
OUT=${1:-docs/screenshots}; mkdir -p "$OUT"
T=/tmp/tailgate-shares-demo; rm -rf $T; mkdir -p $T/cfg $T/smb
ln -s "$PWD/tests/fake-tailscale" $T/tailscale
for n in probe smbclient gio open mount diskutil umount net explorer xdg-open; do
  printf '#!/bin/sh\nexec "%s" %s "$@"\n' "$PWD/tests/fake-smb" $n > $T/smb/$n; chmod +x $T/smb/$n
done
printf '#!/bin/sh\n' > $T/open; chmod +x $T/open
cat > $T/cfg/state.json <<'EOF'
{"shares":[{"host":"100.100.0.5","share":"Media","device":"home-exit","at_login":true},
           {"host":"100.100.0.4","share":"Scans","user":"pat","device":"old-desktop"}]}
EOF
export FAKE_TS_LOG=$T/ts.log FAKE_TS_STATE=$T/state FAKE_TS_MODE=ok FAKE_TS_FIXTURE=$PWD/tests/fixtures/status.json
export FAKE_SMB_LOG=$T/smb.log FAKE_SMB_STATE=$T/smbstate FAKE_SMB_HOSTS="100.100.0.2 100.100.0.5" \
       FAKE_SMB_LIST="100.100.0.5" FAKE_SMB_SHARES="Media Backups Photos"
export TAILGATE_TAILSCALE_BIN=$T/tailscale TAILGATE_OPEN_BIN=$T/open TAILGATE_CONFIG_DIR=$T/cfg TAILGATE_SHARES_TOOLS=$T/smb
N=81; tools/xvfb-run-app.sh $N 1000x780; sleep 4
x() { tools/xclick.py :$N "$@"; }
shot() { sleep "${2:-1.2}"; DISPLAY=:$N import -window root "$OUT/$1.png"; }
x click 699 92; sleep 3; shot shares 0          # Media connected at startup (At login)
x click 906 515; shot shares-connected 2        # Connect Photos
x click 918 200; shot shares-add 1; x key Escape; sleep .4
x click 351 85; shot shares-dark 1; x click 351 85
tools/xvfb-stop.sh $N
grep -vE 'mount	-l' $T/smb.log
