#!/bin/bash
num=$1
kill "$(cat /tmp/tg-app-$num.pid)" 2>/dev/null
kill "$(cat /tmp/tg-xvfb-$num.pid)" 2>/dev/null
rm -f /tmp/tg-app-$num.pid /tmp/tg-xvfb-$num.pid
