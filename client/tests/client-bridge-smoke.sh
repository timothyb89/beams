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
case " $* " in
    *"beam-client-bridge"*)
        exec "$TEST_BOOTSTRAP_BIN" client-bridge --socket "$TEST_VM_SOCKET"
        ;;
esac
exec python3 "$TEST_FAKE_TSH" "$@"
EOF
chmod 700 "$TEST_DIR/bin/tsh"

cat >"$TEST_DIR/fake-tsh.py" <<'PY'
import signal
import socket
import sys

if sys.argv[1] == "scp":
    import os
    destination = sys.argv[-1]
    if "-r" in sys.argv:
        # Like `tsh scp -r`: the source directory always lands *inside* the
        # destination, which is created if it doesn't exist.
        source_name = os.path.basename(sys.argv[-2].rstrip("/"))
        inner = os.path.join(destination, source_name)
        os.makedirs(inner)
        with open(os.path.join(inner, "inner.txt"), "wb") as downloaded:
            downloaded.write(b"beams-fetch-dir-smoke\n")
    else:
        with open(destination, "wb") as downloaded:
            downloaded.write(b"beams-copy-smoke\n")
    raise SystemExit(0)

forward = sys.argv[sys.argv.index("-L") + 1]
bind, port, _remote_host, _remote_port = forward.split(":")
listener = socket.socket()
listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
listener.bind((bind, int(port)))
listener.listen()

def stop(_signal, _frame):
    raise SystemExit(0)

signal.signal(signal.SIGTERM, stop)
while True:
    connection, _ = listener.accept()
    connection.sendall(b"beams-forward-smoke\n")
    connection.close()
PY

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
elif mode in ("reject", "approve", "approve-copy"):
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
    decision = {
        "op": "decide_request",
        "request_id": pending["request_id"],
        "approve": mode != "reject",
    }
    if mode == "approve-copy":
        decision["local_path"] = sys.argv[3]
    send(decision)
    wanted = "rejected" if mode == "reject" else "running"
    while time.time() < deadline:
        event = receive()
        if (
            event.get("type") == "request_changed"
            and event["request"]["request_id"] == pending["request_id"]
            and event["request"]["status"] == wanted
        ):
            break
    else:
        raise RuntimeError(f"decision did not reach {wanted}")
    if mode == "approve" and pending.get("action") is None:
        send({"op": "snapshot"})
        while time.time() < deadline:
            event = receive()
            if event.get("type") == "snapshot":
                running = next(
                    (s for s in event["services"] if s["status"] == "running"),
                    None,
                )
                if running is not None:
                    print(json.dumps(running, sort_keys=True))
                    break
        else:
            raise RuntimeError("running service did not appear")
    elif mode == "approve-copy":
        while time.time() < deadline:
            event = receive()
            if (
                event.get("type") == "service_changed"
                and event["service"]["request_id"] == pending["request_id"]
                and event["service"]["status"] in ("stopped", "error")
            ):
                completed = event["service"]
            elif event.get("type") == "snapshot":
                completed = next(
                    (
                        s
                        for s in event["services"]
                        if s["request_id"] == pending["request_id"]
                        and s["status"] in ("stopped", "error")
                    ),
                    None,
                )
            else:
                completed = None
            if completed is not None:
                if completed["status"] != "stopped":
                    raise RuntimeError(completed.get("message", "copy failed"))
                print(json.dumps(completed, sort_keys=True))
                break
        else:
            raise RuntimeError("file-copy service did not complete")
elif mode == "wait-pending":
    while time.time() < deadline:
        event = receive()
        request = None
        if event.get("type") == "snapshot":
            request = next(
                (r for r in event["requests"] if r["status"] == "pending_approval"),
                None,
            )
        elif (
            event.get("type") == "request_changed"
            and event["request"]["status"] == "pending_approval"
        ):
            request = event["request"]
        if request is not None:
            print(request["request_id"])
            break
    else:
        raise RuntimeError("pending request did not arrive")
elif mode == "wait-request":
    request_id, wanted = sys.argv[3:5]
    while time.time() < deadline:
        event = receive()
        requests = []
        if event.get("type") == "snapshot":
            requests = event["requests"]
        elif event.get("type") == "request_changed":
            requests = [event["request"]]
        match = next(
            (r for r in requests if r["request_id"] == request_id and r["status"] == wanted),
            None,
        )
        if match is not None:
            print(match.get("message") or "")
            break
    else:
        raise RuntimeError(f"request {request_id} did not reach {wanted}")
elif mode == "assert-empty":
    while time.time() < deadline:
        event = receive()
        if event.get("type") == "snapshot":
            assert event["requests"] == [], event
            break
    else:
        raise RuntimeError("snapshot did not arrive")
elif mode == "wait-running":
    while time.time() < deadline:
        send({"op": "snapshot"})
        event = receive()
        while event.get("type") != "snapshot":
            event = receive()
        running = next(
            (s for s in event["services"] if s["status"] == "running"),
            None,
        )
        if running is not None:
            print(json.dumps(running, sort_keys=True))
            break
        time.sleep(0.05)
    else:
        raise RuntimeError("running service did not appear")
elif mode == "dump":
    while time.time() < deadline:
        event = receive()
        if event.get("type") == "snapshot":
            print(json.dumps(event, sort_keys=True))
            break
else:
    raise RuntimeError(f"unknown mode {mode}")
PY

cat >"$TEST_DIR/remote-action.py" <<'PY'
import json
import socket
import sys
import time
import uuid

socket_path, token, client_id, service_id, action = sys.argv[1:6]
request_id = str(uuid.uuid4())
sock = socket.socket(socket.AF_UNIX)
sock.connect(socket_path)
reader = sock.makefile("r")
writer = sock.makefile("w")
writer.write(json.dumps({
    "request_id": "submit-action",
    "op": "request_service_action",
    "session_token": token,
    "client_id": client_id,
    "request": {
        "request_id": request_id,
        "service_id": service_id,
        "action": action,
    },
}) + "\n")
writer.flush()
deadline = time.time() + 15
while time.time() < deadline:
    line = reader.readline()
    if not line:
        raise RuntimeError("VM daemon disconnected")
    event = json.loads(line)
    if event.get("type") == "error" and event.get("request_id") == "submit-action":
        raise RuntimeError(event["message"])
    if (
        event.get("type") == "service_action_request_status"
        and event["request"]["request"]["request_id"] == request_id
    ):
        status = event["request"]["status"]
        if status == "running":
            print(status)
            raise SystemExit(0)
        if status in ("rejected", "expired", "failed"):
            raise RuntimeError(status)
raise RuntimeError("action did not complete")
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
        TEST_FAKE_TSH="$TEST_DIR/fake-tsh.py" \
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
        TEST_FAKE_TSH="$TEST_DIR/fake-tsh.py" \
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

# A remote file-copy proposal requires the local frontend to supply the real
# workstation path. The VM sees only the hint and sanitized lifecycle text,
# while the one-shot command blocks until the transfer itself is terminal.
COPY_DEST="$TEST_DIR/downloaded.txt"
env \
    BEAMS_CLIENT_ID="$(cat "$TEST_DIR/client-b-id")" \
    BEAMS_SESSION_TOKEN=token-b \
    "$ROOT/bootstrap/target/debug/beam-bootstrap" client request \
        --socket "$VM_SOCKET" download --remote-path /tmp/source \
        --local-path-hint suggested.txt \
        >"$TEST_DIR/copy-request.out" 2>&1 &
COPY_REQUEST_PID=$!
python3 "$TEST_DIR/frontend.py" approve-copy "$LOCAL_B_SOCKET" "$COPY_DEST" \
    >"$TEST_DIR/copy-service.json"
wait "$COPY_REQUEST_PID"
grep -q "beams-copy-smoke" "$COPY_DEST"
grep -q "file copy completed" "$TEST_DIR/copy-request.out"
if grep -Fq "$COPY_DEST" "$TEST_DIR/copy-request.out"; then
    echo "remote copy status leaked the workstation path" >&2
    exit 1
fi
python3 "$TEST_DIR/frontend.py" assert-empty "$LOCAL_A_SOCKET"

# `beams fetch` with several sources and a destination that doesn't exist yet:
# the workstation creates the directory and lands every item inside it, and
# the Beam-side command exits 0 once all of them are installed.
mkdir -p "$TEST_DIR/beam-src/logs"
printf 'a\n' >"$TEST_DIR/beam-src/a.log"
printf 'bb\n' >"$TEST_DIR/beam-src/b.log"
printf 'ccc\n' >"$TEST_DIR/beam-src/logs/c.log"
FETCH_DEST="$TEST_DIR/fetched"
(
    cd "$TEST_DIR/beam-src"
    env \
        BEAMS_CLIENT_ID="$(cat "$TEST_DIR/client-b-id")" \
        BEAMS_SESSION_TOKEN=token-b \
        "$ROOT/bootstrap/target/debug/beam-bootstrap" fetch \
            --socket "$VM_SOCKET" a.log b.log logs ./suggested-dest/ \
            >"$TEST_DIR/fetch.out" 2>&1
) &
FETCH_PID=$!
python3 "$TEST_DIR/frontend.py" approve-copy "$LOCAL_B_SOCKET" "$FETCH_DEST" \
    >"$TEST_DIR/fetch-service.json"
wait "$FETCH_PID"
grep -q "beams-copy-smoke" "$FETCH_DEST/a.log"
grep -q "beams-copy-smoke" "$FETCH_DEST/b.log"
grep -q "beams-fetch-dir-smoke" "$FETCH_DEST/logs/inner.txt"
grep -q "fetched 3 items" "$TEST_DIR/fetch.out"
python3 - "$TEST_DIR/fetch-service.json" <<'PY'
import json
import sys
service = json.load(open(sys.argv[1]))
assert service["spec"]["local_path_hint"] == "./suggested-dest/", service
progress = service["progress"]
assert progress["files_done"] == progress["files_total"] == 3, progress
assert progress["bytes_total"] == 9, progress
PY
if ls -a "$FETCH_DEST" | grep -q beams-fetch; then
    echo "fetch left temporary files behind" >&2
    exit 1
fi
if grep -Fq "$FETCH_DEST" "$TEST_DIR/fetch.out"; then
    echo "fetch status leaked the workstation path" >&2
    exit 1
fi

# Ctrl-C while the fetch waits for approval withdraws it on the workstation.
env \
    BEAMS_CLIENT_ID="$(cat "$TEST_DIR/client-b-id")" \
    BEAMS_SESSION_TOKEN=token-b \
    "$ROOT/bootstrap/target/debug/beam-bootstrap" fetch \
        --socket "$VM_SOCKET" "$TEST_DIR/beam-src/a.log" \
        >"$TEST_DIR/withdrawn-fetch.out" 2>&1 &
WITHDRAWN_PID=$!
WITHDRAWN_ID=$(python3 "$TEST_DIR/frontend.py" wait-pending "$LOCAL_B_SOCKET")
# (Background jobs of a non-interactive shell ignore SIGINT, so stand in
# for Ctrl-C with SIGTERM: either way the command's daemon connection closes.)
kill -TERM "$WITHDRAWN_PID"
wait "$WITHDRAWN_PID" 2>/dev/null || true
python3 "$TEST_DIR/frontend.py" wait-request "$LOCAL_B_SOCKET" "$WITHDRAWN_ID" expired \
    >"$TEST_DIR/withdrawn-message"
grep -q "withdrawn on the Beam" "$TEST_DIR/withdrawn-message"

# Approving the same remote request creates a real loopback listener owned by
# client B. Client A must remain an observer.
env \
    BEAMS_CLIENT_ID="$(cat "$TEST_DIR/client-b-id")" \
    BEAMS_SESSION_TOKEN=token-b \
    "$ROOT/bootstrap/target/debug/beam-bootstrap" client request \
        --socket "$VM_SOCKET" port-forward --remote-port 8081 \
        >"$TEST_DIR/approved-request.out" 2>&1 &
APPROVED_REQUEST_PID=$!
python3 "$TEST_DIR/frontend.py" approve "$LOCAL_B_SOCKET" >"$TEST_DIR/service.json"
wait "$APPROVED_REQUEST_PID"
python3 "$TEST_DIR/frontend.py" assert-empty "$LOCAL_A_SOCKET"

SERVICE_ID=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["service_id"])' "$TEST_DIR/service.json")
LOCAL_PORT=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["spec"]["local_port"])' "$TEST_DIR/service.json")
python3 - "$LOCAL_PORT" <<'PY'
import socket
import sys
with socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=2) as connection:
    assert connection.recv(64).startswith(b"beams-forward-smoke")
PY

# Remote stop/restart are proposals too: each is routed to client B and waits
# for a fresh local approval.
python3 "$TEST_DIR/remote-action.py" "$VM_SOCKET" token-b \
    "$(cat "$TEST_DIR/client-b-id")" "$SERVICE_ID" stop \
    >"$TEST_DIR/stop-action.out" 2>&1 &
STOP_PID=$!
python3 "$TEST_DIR/frontend.py" approve "$LOCAL_B_SOCKET"
wait "$STOP_PID"
if python3 - "$LOCAL_PORT" <<'PY'
import socket
import sys
try:
    socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=.2)
except OSError:
    raise SystemExit(1)
PY
then
    echo "stopped forward still accepted connections" >&2
    exit 1
fi

python3 "$TEST_DIR/remote-action.py" "$VM_SOCKET" token-b \
    "$(cat "$TEST_DIR/client-b-id")" "$SERVICE_ID" restart \
    >"$TEST_DIR/restart-action.out" 2>&1 &
RESTART_PID=$!
python3 "$TEST_DIR/frontend.py" approve "$LOCAL_B_SOCKET"
wait "$RESTART_PID"
python3 "$TEST_DIR/frontend.py" wait-running "$LOCAL_B_SOCKET" >"$TEST_DIR/service-restarted.json"

# Restart one workstation daemon. Its stable client ID survives, and a new
# ensure call recreates exactly one bridge for that client/Beam pair and
# restores the desired-running forward without trusting its old PID.
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
python3 "$TEST_DIR/frontend.py" wait-running "$LOCAL_B_SOCKET" >"$TEST_DIR/service-restored.json"
python3 - "$LOCAL_PORT" <<'PY'
import socket
import sys
with socket.create_connection(("127.0.0.1", int(sys.argv[1])), timeout=2) as connection:
    assert connection.recv(64).startswith(b"beams-forward-smoke")
PY

echo "client bridge smoke test passed"
