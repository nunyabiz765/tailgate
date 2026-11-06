#!/bin/sh
# Runs the real install helper against pkgs.tailscale.com inside a throwaway Ubuntu
# container, as root, with the .deb (or the helper and key) already in place. Used by CI's
# deb-test and by `podman run` locally. Never run it on a real machine: it rewrites
# /etc/os-release and installs and removes Tailscale.
#   real-install-test.sh [online|offline]
set -eu
H=/usr/lib/tailgate/install-tailscale
mode=${1:-online}
export DEBIAN_FRONTEND=noninteractive
id suzanne >/dev/null 2>&1 || useradd -m suzanne
uid=$(id -u suzanne)
say() { printf '\n== %s\n' "$*"; }
expect() { # code command...
    want=$1; shift
    set +e; "$@" > /tmp/out.txt 2>&1; rc=$?; set -e
    cat /tmp/out.txt
    [ "$rc" -eq "$want" ] || { echo "FAIL: expected exit $want, got $rc"; tail -n 30 /var/log/tailgate-install.log; exit 1; }
}
base=$(sed -n 's/^UBUNTU_CODENAME=//p' /etc/os-release)
# Look like Zorin OS (17 = jammy, 18 = noble), the way Zorin's own os-release reads.
cp /etc/os-release /tmp/os-release.orig
printf 'NAME="Zorin OS"\nVERSION="test"\nID=zorin\nID_LIKE="ubuntu debian"\nPRETTY_NAME="Zorin OS test"\nVERSION_CODENAME=%s\nUBUNTU_CODENAME=%s\n' "$base" "$base" > /etc/os-release.zorin
rm -f /etc/os-release; cp /etc/os-release.zorin /etc/os-release

if [ "$mode" = offline ]; then
    say "No network: plain 'offline' answer, nothing installed"
    expect 20 env PKEXEC_UID="$uid" "$H"
    grep -q "online" /tmp/out.txt
    ! command -v tailscale
    exit 0
fi

say "Refuses arguments and non-root"
expect 2 "$H" --operator=evil
expect 2 su suzanne -s /bin/sh -c "$H"

say "Fresh install as Zorin ($base): real tailscale from pkgs.tailscale.com"
# No systemd in a container, so tailscaled can't be started: exit 24 after installing.
expect 24 env PKEXEC_UID="$uid" "$H"
grep -q '^step: Downloading Tailscale' /tmp/out.txt
dpkg-query -W -f='${Package} ${Version} ${Status}\n' tailscale tailscale-archive-keyring
grep -qx "deb \[signed-by=/usr/share/keyrings/tailscale-archive-keyring.gpg\] https://pkgs.tailscale.com/stable/ubuntu $base main" /etc/apt/sources.list.d/tailscale.list
test -s /usr/share/keyrings/tailscale-archive-keyring.gpg
tailscale version

say "tailscaled up (userspace, as a container allows), helper again: operator = suzanne"
mkdir -p /run/tailscale /var/lib/tailscale
tailscaled --tun=userspace-networking --state=mem: > /tmp/tailscaled.log 2>&1 &
TSD=$!
expect 0 env PKEXEC_UID="$uid" "$H"
grep -q '^step: Tailscale is already installed' /tmp/out.txt
grep -q '^step: Letting suzanne control Tailscale' /tmp/out.txt
tailscale debug prefs | grep -q '"OperatorUser": "suzanne"'
su suzanne -s /bin/sh -c 'tailscale set --exit-node-allow-lan-access=true'
tailscale debug prefs | grep -q '"ExitNodeAllowLANAccess": true'
su suzanne -s /bin/sh -c 'tailscale status --json' | grep -q '"BackendState": "NeedsLogin"'
kill $TSD; wait $TSD 2>/dev/null || true

say "Software Updater holding the dpkg lock: waits, then a plain 'busy' answer"
apt-get purge -y -qq tailscale tailscale-archive-keyring > /dev/null
rm -f /etc/apt/sources.list.d/tailscale.list /usr/share/keyrings/tailscale-archive-keyring.gpg
command -v python3 >/dev/null || apt-get install -y -qq python3 > /dev/null
python3 -c 'import fcntl,time; f=open("/var/lib/dpkg/lock-frontend","w"); fcntl.lockf(f, fcntl.LOCK_EX); time.sleep(40)' &
LOCKER=$!; sleep 1
expect 21 env PKEXEC_UID="$uid" TAILGATE_LOCK_WAIT=10 "$H"
grep -q '^step: Waiting for another update to finish' /tmp/out.txt
grep -q 'Software Updater' /tmp/out.txt
say "...and when the lock frees up while it waits, it carries on"
kill $LOCKER; wait $LOCKER 2>/dev/null || true
python3 -c 'import fcntl,time; f=open("/var/lib/dpkg/lock-frontend","w"); fcntl.lockf(f, fcntl.LOCK_EX); time.sleep(12)' &
sleep 1
expect 24 env PKEXEC_UID="$uid" TAILGATE_LOCK_WAIT=120 "$H"
grep -q '^step: Waiting for another update to finish' /tmp/out.txt
dpkg-query -W tailscale

say "A release Tailscale doesn't list yet: falls back to the newest Ubuntu base"
apt-get purge -y -qq tailscale tailscale-archive-keyring > /dev/null
rm -f /etc/apt/sources.list.d/tailscale.list
sed -i 's/CODENAME=.*/CODENAME=zzzfuture/' /etc/os-release
expect 24 env PKEXEC_UID="$uid" "$H"
grep -q ' noble main' /etc/apt/sources.list.d/tailscale.list
dpkg-query -W tailscale
grep -q 'no Tailscale list for ubuntu zzzfuture; using ubuntu noble' /var/log/tailgate-install.log

cp /tmp/os-release.orig /etc/os-release
say "PASS: real install helper ($base)"
