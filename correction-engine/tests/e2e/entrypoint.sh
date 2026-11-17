#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

cleanup() {
    [ -n "${APP_PID:-}" ] && kill "$APP_PID" 2>/dev/null || true
    [ -n "${WESTON_PID:-}" ] && kill "$WESTON_PID" 2>/dev/null || true
    [ -n "${REGISTRYD_PID:-}" ] && kill "$REGISTRYD_PID" 2>/dev/null || true
}
trap cleanup EXIT

rm -f /tmp/e2e-subscribed /tmp/e2e-ready /tmp/e2e-do-edit /tmp/e2e-edit-done /tmp/test-app.pid

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/tmp/xdg-runtime}"
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"

eval "$(dbus-launch --sh-syntax)"
export DBUS_SESSION_BUS_ADDRESS

/usr/libexec/at-spi2-registryd &
REGISTRYD_PID=$!
sleep 1

export WAYLAND_DISPLAY=wayland-1
weston --backend=headless-backend.so --no-config --socket="$WAYLAND_DISPLAY" &
WESTON_PID=$!

for _ in $(seq 1 50); do
    [ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ] && break
    sleep 0.2
done
if [ ! -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ]; then
    echo "FATAL: weston Wayland socket never appeared at $XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" >&2
    exit 1
fi

export GTK_A11Y=atspi
unset NO_AT_BRIDGE || true

python3 "$SCRIPT_DIR/test_app.py" &
APP_PID=$!

export CORRECTION_E2E=1

TEST_EXIT=0
cargo test --package correction-engine --test e2e_tests -- --nocapture --test-threads=1 \
    || TEST_EXIT=$?

exit "$TEST_EXIT"
