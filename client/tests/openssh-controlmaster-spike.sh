#!/bin/sh

# Functional compatibility test for carrying Beams operations over one
# OpenSSH ControlMaster. Run this on a workstation with a normal `tsh` profile;
# the identity-file-only environment inside a Beam cannot produce a usable
# `tsh config` for the `beams` login.

set -eu

usage() {
    echo "usage: $0 --config SSH_CONFIG --target LOGIN@HOST" >&2
    exit 2
}

SSH_CONFIG=
TARGET=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --config)
            [ "$#" -ge 2 ] || usage
            SSH_CONFIG=$2
            shift 2
            ;;
        --target)
            [ "$#" -ge 2 ] || usage
            TARGET=$2
            shift 2
            ;;
        -h|--help)
            usage
            ;;
        *)
            usage
            ;;
    esac
done

[ -n "$SSH_CONFIG" ] || usage
[ -f "$SSH_CONFIG" ] || {
    echo "SSH config does not exist: $SSH_CONFIG" >&2
    exit 2
}
[ -n "$TARGET" ] || usage

# Keep the path short for macOS's comparatively small Unix-socket path limit.
TEST_DIR=$(mktemp -d /tmp/beams-cm.XXXXXX)
CONTROL_SOCKET="$TEST_DIR/cm"
REMOTE_DIR="/tmp/beams-cm-$$"
REMOTE_HTTP_PID=
MASTER_STARTED=0

ssh_channel() {
    ssh -F "$SSH_CONFIG" -o ControlPath="$CONTROL_SOCKET" "$TARGET" "$@"
}

cleanup() {
    if [ -n "$REMOTE_HTTP_PID" ]; then
        kill "$REMOTE_HTTP_PID" 2>/dev/null || true
        wait "$REMOTE_HTTP_PID" 2>/dev/null || true
    fi
    if [ "$MASTER_STARTED" -eq 1 ]; then
        ssh_channel "rm -rf '$REMOTE_DIR'" >/dev/null 2>&1 || true
        ssh -F "$SSH_CONFIG" -S "$CONTROL_SOCKET" -O exit "$TARGET" \
            >/dev/null 2>&1 || true
    fi
    rm -rf "$TEST_DIR"
}
trap cleanup EXIT HUP INT TERM

start_master() {
    ssh \
        -F "$SSH_CONFIG" \
        -o ControlMaster=yes \
        -o ControlPath="$CONTROL_SOCKET" \
        -o ControlPersist=no \
        -o ExitOnForwardFailure=yes \
        -MNf "$TARGET"
    MASTER_STARTED=1
    ssh -F "$SSH_CONFIG" -S "$CONTROL_SOCKET" -O check "$TARGET"
}

free_local_port() {
    python3 -c \
        'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()'
}

echo "starting OpenSSH master for $TARGET"
start_master

test "$(ssh_channel 'printf channel-ok')" = "channel-ok"
echo "remote command channel: ok"

PTY_OUTPUT=$(
    ssh \
        -tt \
        -F "$SSH_CONFIG" \
        -o ControlPath="$CONTROL_SOCKET" \
        "$TARGET" \
        'test -t 0 && test -t 1 && printf pty-ok' \
        </dev/null 2>/dev/null |
        tr -d '\r'
)
test "$PTY_OUTPUT" = "pty-ok"
echo "PTY channel: ok"

ssh_channel 'sleep 2; printf concurrent-ok' >"$TEST_DIR/concurrent" &
CONCURRENT_PID=$!
test "$(ssh_channel 'printf quick-ok')" = "quick-ok"
wait "$CONCURRENT_PID"
test "$(cat "$TEST_DIR/concurrent")" = "concurrent-ok"
echo "concurrent channels: ok"

# Mutagen currently adds ControlMaster=no. That prevents creation of a new
# master but does not prevent attachment to this existing ControlPath.
test "$(
    ssh \
        -F "$SSH_CONFIG" \
        -o ControlMaster=no \
        -o ControlPath="$CONTROL_SOCKET" \
        "$TARGET" \
        'printf mutagen-options-ok'
)" = "mutagen-options-ok"
echo "Mutagen-style ControlMaster=no attachment: ok"

ssh_channel "mkdir -p '$REMOTE_DIR'"
printf 'controlmaster-copy\n' >"$TEST_DIR/upload"
scp \
    -q \
    -F "$SSH_CONFIG" \
    -o ControlPath="$CONTROL_SOCKET" \
    "$TEST_DIR/upload" "$TARGET:$REMOTE_DIR/upload"
scp \
    -q \
    -F "$SSH_CONFIG" \
    -o ControlPath="$CONTROL_SOCKET" \
    "$TARGET:$REMOTE_DIR/upload" "$TEST_DIR/download"
cmp "$TEST_DIR/upload" "$TEST_DIR/download"
echo "SCP channels: ok"

REMOTE_PORT=$(ssh_channel \
    "python3 -c 'import socket; s=socket.socket(); s.bind((\"127.0.0.1\", 0)); print(s.getsockname()[1]); s.close()'")
LOCAL_PORT=$(free_local_port)
ssh_channel \
    "exec python3 -m http.server '$REMOTE_PORT' --bind 127.0.0.1 --directory '$REMOTE_DIR'" \
    >"$TEST_DIR/http.log" 2>&1 &
REMOTE_HTTP_PID=$!

ATTEMPTS=0
while ! ssh \
    -F "$SSH_CONFIG" \
    -S "$CONTROL_SOCKET" \
    -O forward \
    -L "127.0.0.1:$LOCAL_PORT:127.0.0.1:$REMOTE_PORT" \
    "$TARGET" 2>"$TEST_DIR/forward-error"; do
    ATTEMPTS=$((ATTEMPTS + 1))
    if [ "$ATTEMPTS" -ge 20 ]; then
        cat "$TEST_DIR/forward-error" >&2
        exit 1
    fi
    sleep 0.1
done

ATTEMPTS=0
while ! curl -fsS --max-time 2 "http://127.0.0.1:$LOCAL_PORT/upload" \
    >"$TEST_DIR/forwarded"; do
    ATTEMPTS=$((ATTEMPTS + 1))
    if [ "$ATTEMPTS" -ge 50 ]; then
        echo "forwarded HTTP server did not become ready" >&2
        exit 1
    fi
    sleep 0.1
done
cmp "$TEST_DIR/upload" "$TEST_DIR/forwarded"

ssh \
    -F "$SSH_CONFIG" \
    -S "$CONTROL_SOCKET" \
    -O cancel \
    -L "127.0.0.1:$LOCAL_PORT:127.0.0.1:$REMOTE_PORT" \
    "$TARGET"
if curl -fsS --max-time 1 "http://127.0.0.1:$LOCAL_PORT/upload" >/dev/null 2>&1; then
    echo "cancelled forward is still accepting connections" >&2
    exit 1
fi
echo "dynamic local forward and cancellation: ok"

python3 - "$SSH_CONFIG" "$CONTROL_SOCKET" "$TARGET" <<'PY'
import statistics
import subprocess
import sys
import time

config, control_socket, target = sys.argv[1:]
samples = []
for _ in range(5):
    start = time.perf_counter()
    subprocess.run(
        [
            "ssh",
            "-F",
            config,
            "-o",
            f"ControlPath={control_socket}",
            target,
            "true",
        ],
        check=True,
    )
    samples.append((time.perf_counter() - start) * 1000)
print(f"warm command channels: median {statistics.median(samples):.1f} ms ({len(samples)} runs)")
PY

ssh -F "$SSH_CONFIG" -S "$CONTROL_SOCKET" -O exit "$TARGET"
MASTER_STARTED=0
ATTEMPTS=0
while [ -e "$CONTROL_SOCKET" ]; do
    ATTEMPTS=$((ATTEMPTS + 1))
    if [ "$ATTEMPTS" -ge 50 ]; then
        echo "master socket remained after exit" >&2
        exit 1
    fi
    sleep 0.1
done
echo "master shutdown: ok"

start_master
test "$(ssh_channel 'printf recreated-ok')" = "recreated-ok"
echo "master recreation: ok"

echo "OpenSSH ControlMaster compatibility spike passed"
