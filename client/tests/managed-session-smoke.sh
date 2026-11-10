#!/bin/sh

# End-to-end smoke test for the outer-terminal side of `client/src/session.rs`.
# The Rust unit tests exercise the inner PTY directly; this adds a real
# `script(1)`-allocated controlling terminal around the compiled `beams`
# process, with a fake `tsh` standing in for the remote endpoint.

set -eu
# An inherited agent with keys (e.g. a Beam's relay) would put up the
# one-time "share an SSH key?" toast, and Ctrl-] would open that instead of
# the menu this test expects.
unset SSH_AUTH_SOCK

CLIENT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
TEST_DIR=$(mktemp -d "${TMPDIR:-/tmp}/beams-managed-session.XXXXXX")
TRANSCRIPT="$TEST_DIR/transcript"
DAEMON_PID=
SESSION_FLAGS=
if [ "${BEAMS_TEST_OVERLAY:-0}" = 1 ]; then
    SESSION_FLAGS=--terminal-overlay
fi
cleanup() {
    if [ -n "$DAEMON_PID" ]; then
        kill "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
    fi
    if [ "${BEAMS_TEST_KEEP:-0}" != 1 ]; then
        rm -rf "$TEST_DIR"
    fi
}
trap cleanup EXIT

cargo build --manifest-path "$CLIENT_DIR/Cargo.toml"
# A profile launch stages the Beam runtime, even though the fake tsh drops it.
cargo build -q --manifest-path "$CLIENT_DIR/../bootstrap/Cargo.toml"

mkdir -p "$TEST_DIR/config/beams-client" "$TEST_DIR/runtime"
cp "$CLIENT_DIR/tests/fixtures/config/beams-client/state.json" \
    "$TEST_DIR/config/beams-client/state.json"

env \
    XDG_CONFIG_HOME="$TEST_DIR/config" \
    XDG_RUNTIME_DIR="$TEST_DIR/runtime" \
    "$CLIENT_DIR/target/debug/beams" daemon >"$TEST_DIR/daemon.log" 2>&1 &
DAEMON_PID=$!

SOCKET="$TEST_DIR/runtime/beams-client/daemon.sock"
ATTEMPTS=0
while [ ! -S "$SOCKET" ]; do
    ATTEMPTS=$((ATTEMPTS + 1))
    if [ "$ATTEMPTS" -ge 100 ]; then
        echo "local daemon did not create $SOCKET" >&2
        exit 1
    fi
    sleep 0.05
done

set +e
# zellij enables enhanced keyboard reporting on capable outer terminals.
# Exercise Ctrl-] in its Kitty CSI-u form, then open/cancel the forward modal,
# then type a partial workstation path into the download form so the path
# dropdown lists the matching directory (Esc hides it, Esc cancels).
mkdir -p "$TEST_DIR/picker/Downloads-smoke"
{
    # Entry probes finish before remote output is forwarded. Synchronize on
    # that handoff so fixed typing delays do not race the capture window.
    INPUT_ATTEMPTS=0
    while ! grep -q "REMOTE READY" "$TRANSCRIPT" 2>/dev/null; do
        INPUT_ATTEMPTS=$((INPUT_ATTEMPTS + 1))
        if [ "$INPUT_ATTEMPTS" -ge 200 ]; then
            echo "managed session did not forward its first remote frame" >&2
            exit 1
        fi
        sleep 0.01
    done
    printf 'hello\r\033[93;5u'
    sleep 0.1
    printf 'f'
    sleep 0.1
    printf '8080\t18080\033'
    sleep 0.1
    printf 'd'
    sleep 0.1
    printf '/remote/file\t%s/dsmk' "$TEST_DIR/picker"
    sleep 0.2
    printf '\033'
    sleep 0.1
    printf '\033'
    sleep 0.1
    printf 'q'
} |
    env \
        PATH="$CLIENT_DIR/tests/fixtures/bin:$PATH" \
        XDG_CONFIG_HOME="$TEST_DIR/config" \
        XDG_RUNTIME_DIR="$TEST_DIR/runtime" \
        TERM=xterm-256color \
        script -qefc \
        "stty rows 24 cols 100; exec \"$CLIENT_DIR/target/debug/beams\" ssh $SESSION_FLAGS --purpose managed-session-test" \
        "$TRANSCRIPT" >/dev/null
STATUS=$?
set -e

if [ "$STATUS" -ne 7 ]; then
    echo "expected managed child exit code 7, got $STATUS" >&2
    exit 1
fi

# Both modes share one renderer: the home dialog, then the forward form
# with its "automatic" local-port placeholder.
strings "$TRANSCRIPT" | grep -q "Beams local menu"
strings "$TRANSCRIPT" | grep -q "Forward a Beam port"
strings "$TRANSCRIPT" | grep -q "automatic"
# Match highlights style characters individually, so drop SGR sequences
# before looking for the dropdown entry.
sed 's/\x1b\[[0-9;]*m//g' "$TRANSCRIPT" | grep -q "Downloads-smoke/"
strings "$TRANSCRIPT" | grep -q "REMOTE READY"
strings "$TRANSCRIPT" | grep -q "REMOTE TITLE"
strings "$TRANSCRIPT" | grep -q "24 100"
strings "$TRANSCRIPT" | grep -q "REMOTE ECHO: hello"
if [ "${BEAMS_TEST_OVERLAY:-0}" != 1 ]; then
    strings "$TRANSCRIPT" | grep -q "REMOTE RESIZE: 24 99"
    strings "$TRANSCRIPT" | grep -q "REMOTE RESIZE: 24 100"
fi
CLIENT_ID=$(sed -n 's/.*"client_id": "\([^"]*\)".*/\1/p' \
    "$TEST_DIR/config/beams-client/daemon-state.json")
test -n "$CLIENT_ID"
strings "$TRANSCRIPT" | grep -q "$CLIENT_ID"

echo "managed-session smoke test passed"
