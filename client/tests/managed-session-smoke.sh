#!/bin/sh

# End-to-end smoke test for the outer-terminal side of `client/src/session.rs`.
# The Rust unit tests exercise the inner PTY directly; this adds a real
# `script(1)`-allocated controlling terminal around the compiled `beams`
# process, with a fake `tsh` standing in for the remote endpoint.

set -eu

CLIENT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
TEST_DIR=$(mktemp -d "${TMPDIR:-/tmp}/beams-managed-session.XXXXXX")
TRANSCRIPT="$TEST_DIR/transcript"
DAEMON_PID=
cleanup() {
    if [ -n "$DAEMON_PID" ]; then
        kill "$DAEMON_PID" 2>/dev/null || true
        wait "$DAEMON_PID" 2>/dev/null || true
    fi
    rm -rf "$TEST_DIR"
}
trap cleanup EXIT

cargo build --manifest-path "$CLIENT_DIR/Cargo.toml"

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
printf 'hello\r\035q' |
    env \
        PATH="$CLIENT_DIR/tests/fixtures/bin:$PATH" \
        XDG_CONFIG_HOME="$TEST_DIR/config" \
        XDG_RUNTIME_DIR="$TEST_DIR/runtime" \
        TERM=xterm-256color \
        script -qefc \
        "stty rows 24 cols 100; exec \"$CLIENT_DIR/target/debug/beams\" ssh --purpose managed-session-test" \
        "$TRANSCRIPT" >/dev/null
STATUS=$?
set -e

if [ "$STATUS" -ne 7 ]; then
    echo "expected managed child exit code 7, got $STATUS" >&2
    exit 1
fi

strings "$TRANSCRIPT" | grep -q "Beams local menu"
strings "$TRANSCRIPT" | grep -q "REMOTE READY"
strings "$TRANSCRIPT" | grep -q "24 100"
strings "$TRANSCRIPT" | grep -q "REMOTE ECHO: hello"
CLIENT_ID=$(sed -n 's/.*"client_id": "\([^"]*\)".*/\1/p' \
    "$TEST_DIR/config/beams-client/daemon-state.json")
test -n "$CLIENT_ID"
strings "$TRANSCRIPT" | grep -q "$CLIENT_ID"

echo "managed-session smoke test passed"
