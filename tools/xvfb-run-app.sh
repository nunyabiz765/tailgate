#!/bin/bash
# Starts Tailgate on a private Xvfb display so it never appears on the desktop.
# usage: xvfb-run-app.sh DISPLAYNUM WxH [app args]; app env passes through.
num=$1; size=$2; shift 2
Xvfb ":$num" -screen 0 "${size}x24" -nolisten tcp >/dev/null 2>&1 &
echo $! > "/tmp/tg-xvfb-$num.pid"
sleep 0.7
env -u WAYLAND_DISPLAY DISPLAY=":$num" XDG_SESSION_TYPE=x11 SLINT_BACKEND="${SLINT_BACKEND:-winit-software}" \
  "${TAILGATE_EXE:-$(dirname "$0")/../target/debug/tailgate}" "$@" >"/tmp/tg-app-$num.log" 2>&1 &
echo $! > "/tmp/tg-app-$num.pid"
