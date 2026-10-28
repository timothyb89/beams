#!/bin/sh

# End-to-end Phase 3 control-plane test without a real Beam. A fake `tsh`
# executes the real `beam-bootstrap client-bridge` locally, preserving the
# exact stdio protocol and both real daemon UDS protocols.

set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
TEST_DIR=$(mktemp -d "${TMPDIR:-/tmp}/beams-client-bridge.XXXXXX")
VM_SOCKET="$TEST_DIR/vm.sock"
LOCAL_A_SOCKET="$TEST_DIR/local-a.sock"
LOCAL_B_SOCKET="$TEST_DIR/local-b.sock"
VM_PID=
LOCAL_A_PID=
LOCAL_B_PID=

cleanup() {
    for pid in "$LOCAL_B_PID" "$LOCAL_A_PID" "$VM_PID"; do
        if [ -n "$pid" ]; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    if [ "${KEEP_TEST_DIR:-0}" = 1 ]; then
        echo "kept smoke-test directory: $TEST_DIR" >&2
    else
        rm -rf "$TEST_DIR"
    fi
}
trap cleanup EXIT

cargo build --manifest-path "$ROOT/bootstrap/Cargo.toml"
cargo build --manifest-path "$ROOT/client/Cargo.toml"

mkdir -p "$TEST_DIR/bin" "$TEST_DIR/config-a" "$TEST_DIR/config-b"
cat >"$TEST_DIR/bin/tsh" <<'EOF'
#!/bin/sh
exec "$TEST_BOOTSTRAP_BIN" client-bridge --socket "$TEST_VM_SOCKET"
EOF
chmod 700 "$TEST_DIR/bin/tsh"

cat >"$TEST_DIR/frontend.py" <<'PY'
import json
import socket
import sys
import time

mode, socket_path = sys.argv[1:3]
sock = socket.socket(socket.AF_UNIX)
deadline = time.time() + 10
while True:
    try:
        sock.connect(socket_path)
        break
    except OSError:
        if time.time() >= deadline:
            raise
        time.sleep(0.05)

reader = sock.makefile("r")
writer = sock.makefile("w")

def send(value):
    writer.write(json.dumps(value) + "\n")
    writer.flush()

def receive():
    line = reader.readline()
    if not line:
        raise RuntimeError("local daemon disconnected")
    return json.loads(line)

send({
    "op": "hello",
    "protocol": "beams-client-services",
    "version": 1,
    "role": "frontend",
})
hello = receive()
assert hello["type"] == "hello", hello

if mode == "ensure":
    beam_id, beam_uuid, token = sys.argv[3:6]
    send({
        "op": "ensure_bridge",
        "beam_id": beam_id,
        "beam_uuid": beam_uuid,
        "login": "beams",
        "session_token": token,
    })
    while time.time() < deadline:
        event = receive()
        if (
            event.get("type") == "bridge_status"
            and event["bridge"]["beam_uuid"] == beam_uuid
            and event["bridge"]["status"] == "connected"
        ):
            print(hello["client_id"])
            break
    else:
        raise RuntimeError("bridge did not connect")
elif mode == "reject":
    pending = None
    while time.time() < deadline and pending is None:
        event = receive()
        if event.get("type") == "snapshot":
            pending = next(
                (r for r in event["requests"] if r["status"] == "pending_approval"),
                None,
            )
        elif (
            event.get("type") == "request_changed"
            and event["request"]["status"] == "pending_approval"
        ):
            pending = event["request"]
    if pending is None:
        raise RuntimeError("pending request did not arrive")
    send({
        "op": "decide_request",
        "request_id": pending["request_id"],
        "approve": False,
    })
    while time.time() < deadline:
        event = receive()
        if (
            event.get("type") == "request_changed"
            and event["request"]["request_id"] == pending["request_id"]
            and event["request"]["status"] == "rejected"
        ):
            break
    else:
        raise RuntimeError("rejection was not committed")
elif mode == "assert-empty":
    while time.time() < deadline:
        event = receive()
        if event.get("type") == "snapshot":
            assert event["requests"] == [], event
            break
    else:
        raise RuntimeError("snapshot did not arrive")
elif mode == "dump":
    while time.time() < deadline:
        event = receive()
        if event.get("type") == "snapshot":
            print(json.dumps(event, sort_keys=True))
            break
else:
    raise RuntimeError(f"unknown mode {mode}")
PY

start_vm() {
    "$ROOT/bootstrap/target/debug/beam-bootstrap" daemon \
        --socket "$VM_SOCKET" >"$TEST_DIR/vm.log" 2>&1 &
    VM_PID=$!
}

start_local_a() {
    env \
        PATH="$TEST_DIR/bin:$PATH" \
        XDG_CONFIG_HOME="$TEST_DIR/config-a" \
        TEST_BOOTSTRAP_BIN="$ROOT/bootstrap/target/debug/beam-bootstrap" \
        TEST_VM_SOCKET="$VM_SOCKET" \
        "$ROOT/client/target/debug/beams" daemon --socket "$LOCAL_A_SOCKET" \
        >"$TEST_DIR/local-a.log" 2>&1 &
    LOCAL_A_PID=$!
}

start_local_b() {
    env \
        PATH="$TEST_DIR/bin:$PATH" \
        XDG_CONFIG_HOME="$TEST_DIR/config-b" \
        TEST_BOOTSTRAP_BIN="$ROOT/bootstrap/target/debug/beam-bootstrap" \
        TEST_VM_SOCKET="$VM_SOCKET" \
        "$ROOT/client/target/debug/beams" daemon --socket "$LOCAL_B_SOCKET" \
        >"$TEST_DIR/local-b.log" 2>&1 &
    LOCAL_B_PID=$!
}

wait_for_socket() {
    socket_path=$1
    attempts=0
    while [ ! -S "$socket_path" ]; do
        attempts=$((attempts + 1))
        if [ "$attempts" -ge 200 ]; then
            echo "socket did not appear: $socket_path" >&2
            exit 1
        fi
        sleep 0.05
    done
}

wait_for_clients() {
    expected=$1
    attempts=0
    while :; do
        count=$(
            "$ROOT/bootstrap/target/debug/beam-bootstrap" client list \
                --socket "$VM_SOCKET" 2>/dev/null |
                grep -c ' status=connected' || true
        )
        if [ "$count" -eq "$expected" ]; then
            return
        fi
        attempts=$((attempts + 1))
        if [ "$attempts" -ge 200 ]; then
            echo "expected $expected connected clients, got $count" >&2
            "$ROOT/bootstrap/target/debug/beam-bootstrap" client list \
                --socket "$VM_SOCKET" >&2 || true
            python3 "$TEST_DIR/frontend.py" dump "$LOCAL_A_SOCKET" >&2 || true
            python3 "$TEST_DIR/frontend.py" dump "$LOCAL_B_SOCKET" >&2 || true
            exit 1
        fi
        sleep 0.05
    done
}

start_vm
wait_for_socket "$VM_SOCKET"
start_local_a
start_local_b
wait_for_socket "$LOCAL_A_SOCKET"
wait_for_socket "$LOCAL_B_SOCKET"

python3 "$TEST_DIR/frontend.py" ensure "$LOCAL_A_SOCKET" beam-a beam-uuid token-a \
    >"$TEST_DIR/client-a-id"
python3 "$TEST_DIR/frontend.py" ensure "$LOCAL_B_SOCKET" beam-a beam-uuid token-b \
    >"$TEST_DIR/client-b-id"
echo "checking initial bridge synchronization"
wait_for_clients 2

# A VM-daemon restart loses only its projection. Both workstation
# supervisors reconnect with new bridge IDs and resend their snapshots.
kill "$VM_PID"
wait "$VM_PID" 2>/dev/null || true
VM_PID=
start_vm
wait_for_socket "$VM_SOCKET"
echo "checking bridge synchronization after VM daemon restart"
wait_for_clients 2

set +e
env \
    BEAMS_CLIENT_ID="$(cat "$TEST_DIR/client-b-id")" \
    BEAMS_SESSION_TOKEN=token-b \
    "$ROOT/bootstrap/target/debug/beam-bootstrap" client request \
        --socket "$VM_SOCKET" port-forward --remote-port 8080 \
        >"$TEST_DIR/request.out" 2>&1 &
REQUEST_PID=$!
set -e

python3 "$TEST_DIR/frontend.py" reject "$LOCAL_B_SOCKET"
python3 "$TEST_DIR/frontend.py" assert-empty "$LOCAL_A_SOCKET"

set +e
wait "$REQUEST_PID"
REQUEST_STATUS=$?
set -e
if [ "$REQUEST_STATUS" -ne 1 ]; then
    echo "rejected remote request should exit 1, got $REQUEST_STATUS" >&2
    exit 1
fi
grep -q "submitted" "$TEST_DIR/request.out"
grep -q "rejected" "$TEST_DIR/request.out"

# Restart one workstation daemon. Its stable client ID survives, and a new
# ensure call recreates exactly one bridge for that client/Beam pair.
kill "$LOCAL_B_PID"
wait "$LOCAL_B_PID" 2>/dev/null || true
LOCAL_B_PID=
start_local_b
wait_for_socket "$LOCAL_B_SOCKET"
python3 "$TEST_DIR/frontend.py" ensure "$LOCAL_B_SOCKET" beam-a beam-uuid token-b2 \
    >"$TEST_DIR/client-b-id-after-restart"
cmp "$TEST_DIR/client-b-id" "$TEST_DIR/client-b-id-after-restart"
echo "checking bridge synchronization after workstation daemon restart"
wait_for_clients 2

echo "client bridge smoke test passed"
