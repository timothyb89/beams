#!/bin/sh

# End-to-end smoke test for the outer-terminal side of `client/src/session.rs`.
# The Rust unit tests exercise the inner PTY directly; this adds a real
# `script(1)`-allocated controlling terminal around the compiled `beams`
# process, with a fake `tsh` standing in for the remote endpoint.

set -eu

CLIENT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
TRANSCRIPT=$(mktemp "${TMPDIR:-/tmp}/beams-managed-session.XXXXXX")
trap 'rm -f "$TRANSCRIPT"' EXIT

cargo build --manifest-path "$CLIENT_DIR/Cargo.toml"

set +e
printf 'hello\r\035q' |
    env \
        PATH="$CLIENT_DIR/tests/fixtures/bin:$PATH" \
        XDG_CONFIG_HOME="$CLIENT_DIR/tests/fixtures/config" \
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

echo "managed-session smoke test passed"
