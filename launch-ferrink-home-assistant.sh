#!/bin/sh

# Run the native Home Assistant dashboard and restore the Kindle UI on exit.

BIN="ferrink-home-assistant"
APP="/mnt/us/$BIN"
CONFIG="/var/local/ferrink-home-assistant.env"
LOG_DIR="/mnt/us/$BIN-logs"
PID_FILE="/var/run/$BIN-launcher.pid"
STOPPED_AWESOME=0
STOPPED_CVM=0
CLEANED_UP=0
FRONTLIGHT_MONITOR_PID=""

set_frontlight_for_power() {
    charging=$(lipc-get-prop com.lab126.powerd isCharging 2>/dev/null || echo 0)
    if [ "$charging" = "1" ]; then
        desired=16
    else
        desired=0
    fi

    current=$(lipc-get-prop com.lab126.powerd flIntensity 2>/dev/null || echo -1)
    if [ "$current" != "$desired" ]; then
        lipc-set-prop com.lab126.powerd flIntensity "$desired" 2>/dev/null || true
    fi
}

monitor_frontlight() {
    while :; do
        set_frontlight_for_power
        sleep 15
    done
}

cleanup() {
    if [ "$CLEANED_UP" -eq 1 ]; then
        return
    fi
    CLEANED_UP=1
    rm -f "$PID_FILE"

    if [ -n "$FRONTLIGHT_MONITOR_PID" ]; then
        kill "$FRONTLIGHT_MONITOR_PID" 2>/dev/null || true
        wait "$FRONTLIGHT_MONITOR_PID" 2>/dev/null || true
    fi
    set_frontlight_for_power

    if [ "$STOPPED_CVM" -eq 1 ]; then
        killall -CONT cvm 2>/dev/null || true
    fi
    if [ "$STOPPED_AWESOME" -eq 1 ]; then
        killall -CONT awesome 2>/dev/null || true
    fi
    lipc-set-prop com.lab126.powerd preventScreenSaver 0 2>/dev/null || true
    lipc-set-prop com.lab126.pillow disableEnablePillow enable 2>/dev/null || true
}

finish() {
    status=$?
    trap - 0 1 2 15
    cleanup
    exit "$status"
}

trap finish 0 1 2 15

if [ ! -x "$APP" ]; then
    echo "error: $APP is missing or not executable" >&2
    exit 1
fi
if [ ! -r "$CONFIG" ]; then
    echo "error: $CONFIG is missing" >&2
    exit 1
fi

set -a
. "$CONFIG"
set +a

mkdir -p "$LOG_DIR"
RUN_TS=$(date +%Y%m%dT%H%M%S)
LOG_FILE="$LOG_DIR/$RUN_TS.log"
export RUN_TS

lipc-set-prop com.lab126.cmd wirelessEnable 1 2>/dev/null || true
lipc-set-prop com.lab126.powerd preventScreenSaver 1 2>/dev/null || true
lipc-set-prop com.lab126.pillow disableEnablePillow disable 2>/dev/null || true
set_frontlight_for_power
monitor_frontlight &
FRONTLIGHT_MONITOR_PID=$!

if pidof awesome >/dev/null 2>&1; then
    killall -STOP awesome 2>/dev/null || true
    STOPPED_AWESOME=1
fi
if pidof cvm >/dev/null 2>&1; then
    killall -STOP cvm 2>/dev/null || true
    STOPPED_CVM=1
fi

usleep 300000
echo "Starting $APP; log: $LOG_FILE"
"$APP" >"$LOG_FILE" 2>&1
