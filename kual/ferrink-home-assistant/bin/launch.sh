#!/bin/sh

LAUNCHER="/mnt/us/launch-ferrink-home-assistant.sh"
PID_FILE="/var/run/ferrink-home-assistant-launcher.pid"

if [ ! -x "$LAUNCHER" ]; then
    lipc-set-prop com.lab126.appmgrd start app://com.lab126.booklet.home 2>/dev/null || true
    exit 1
fi

if [ -s "$PID_FILE" ]; then
    existing_pid=$(cat "$PID_FILE" 2>/dev/null)
    if [ -n "$existing_pid" ] && kill -0 "$existing_pid" 2>/dev/null; then
        exit 0
    fi
    rm -f "$PID_FILE"
fi

start-stop-daemon -S -b -m -p "$PID_FILE" -x "$LAUNCHER"
