#!/bin/sh
# The whole first-run flow with nothing faked but the password box: the installed .deb's
# Tailgate, as an ordinary user on Xvfb, clicks Install Tailscale; the real helper installs
# real Tailscale from pkgs.tailscale.com and makes that user the operator; Sign in gets a
# real login URL from Tailscale. Runs as root in a throwaway Ubuntu container after
# real-install-test.sh (CI deb-test). pkexec needs a polkit agent and a person, so `sudo -n` stands in for it; containers have no systemd, so tailscaled is
# started by hand (userspace networking) where the desktop would start it as a service.
#   real-app-install-test.sh SHOTDIR
set -eu
S=$1; mkdir -p "$S"; chmod 777 "$S"
export DEBIAN_FRONTEND=noninteractive
apt-get install -y -qq --no-install-recommends sudo xvfb xauth imagemagick libxtst6 libx11-6 python3 > /dev/null
id suzanne >/dev/null 2>&1 || useradd -m suzanne
echo 'suzanne ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/suzanne; chmod 440 /etc/sudoers.d/suzanne
printf '#!/bin/sh\nexec sudo -n "$@"\n' > /usr/local/bin/pkexec-standin
printf '#!/bin/sh\necho "$@" >> /tmp/open.log\n' > /usr/local/bin/open-standin
chmod 755 /usr/local/bin/pkexec-standin /usr/local/bin/open-standin
# Start clean: no Tailscale, no daemon.
pkill tailscaled 2>/dev/null || true
apt-get purge -y -qq tailscale tailscale-archive-keyring > /dev/null 2>&1 || true
rm -f /etc/apt/sources.list.d/tailscale.list /usr/share/keyrings/tailscale-archive-keyring.gpg /var/log/tailgate-install.log /tmp/open.log
! command -v tailscale

N=77
Xvfb ":$N" -screen 0 1000x780x24 -nolisten tcp -ac > /dev/null 2>&1 &
XV=$!; sleep 1
su suzanne -s /bin/sh -c "DISPLAY=:$N SLINT_BACKEND=winit-software TAILGATE_PKEXEC_BIN=/usr/local/bin/pkexec-standin \
    TAILGATE_OPEN_BIN=/usr/local/bin/open-standin TAILGATE_CONFIG_DIR=/tmp/tg-cfg tailgate > /tmp/tailgate.log 2>&1" &
sleep 5
click() { python3 "$(dirname "$0")/xclick.py" ":$N" click "$1" "$2"; }
shot() { DISPLAY=:$N import -window root "$S/$1.png"; echo "shot $1"; }
wait_for() { # seconds, shell condition
    t=$1; shift
    while [ "$t" -gt 0 ]; do sh -c "$*" && return 0; sleep 1; t=$((t - 1)); done
    echo "timed out waiting for: $*"; tail -n 40 /var/log/tailgate-install.log /tmp/tailgate.log; exit 1
}

# Stands in for systemd: starts tailscaled as soon as the package puts it there, the way
# the package's own `systemctl enable --now tailscaled` does on a desktop.
mkdir -p /run/tailscale /var/lib/tailscale
( while [ ! -x /usr/sbin/tailscaled ]; do sleep 0.5; done; sleep 1
  exec /usr/sbin/tailscaled --tun=userspace-networking --statedir=/var/lib/tailscale > /tmp/tailscaled.log 2>&1 ) &

shot real-1-not-installed
click 503 287                                   # Install Tailscale
sleep 4; shot real-2-installing
wait_for 600 "grep -qE 'step: Done|FAILED' /var/log/tailgate-install.log"
grep -q 'step: Done' /var/log/tailgate-install.log
dpkg-query -W tailscale
tailscale debug prefs | grep -q '"OperatorUser": "suzanne"'
sleep 9; shot real-3-sign-in
click 513 350                                   # Sign in to Tailscale
wait_for 90 "grep -q '^https://login.tailscale.com/' /tmp/open.log"
echo "login URL opened: $(cut -c1-34 /tmp/open.log)..."
sleep 2; shot real-4-after-sign-in-click
kill $XV 2>/dev/null || true
echo "PASS: one click installed and started real Tailscale and made suzanne operator; Sign in opened a real login URL"
