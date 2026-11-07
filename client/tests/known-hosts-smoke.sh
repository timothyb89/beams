#!/usr/bin/env bash
# End-to-end ssh KnownHostsCommand lookups without a real Beam: the real
# workstation daemon (with a scratch HOME whose ~/.ssh/known_hosts is the
# trust root), a fake `tsh` running `beam-bootstrap client-bridge` locally,
# and the real Beam daemon. `beam-bootstrap client known-hosts HOST` must
# print exactly the workstation's entries for HOST (plain, hashed,
# [host]:port, @revoked), nothing for an unknown host, and fail quietly
# (exit 0, one stderr line) without a workstation. Scratch directory only;
# this machine's own daemons are never touched. Prints PASS/FAIL per check.
set -uo pipefail

ROOT=$(cd -- "$(dirname -- "$0")/../.." && pwd)
CARGO=${CARGO:-cargo}
T=$(mktemp -d "${TMPDIR:-/tmp}/beams-known-hosts.XXXXXX")
fails=0
pids=()
pass() { echo "PASS $*"; }
fail() { echo "FAIL $*"; fails=$((fails + 1)); }
check() { local name=$1; shift; if "$@"; then pass "$name"; else fail "$name"; fi; }
cleanup() {
    for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null; done
    if [ "${KEEP_TEST_DIR:-0}" = 1 ] || [ "$fails" -gt 0 ]; then
        echo "kept $T"
    else
        rm -rf "$T"
    fi
}
trap cleanup EXIT

"$CARGO" build -q --manifest-path "$ROOT/bootstrap/Cargo.toml" || exit 1
"$CARGO" build -q --manifest-path "$ROOT/client/Cargo.toml" || exit 1
B="$ROOT/bootstrap/target/debug/beam-bootstrap"
C="$ROOT/client/target/debug/beams"
VM_SOCK="$T/vm/daemon.sock"
WS_SOCK="$T/ws-run/daemon.sock"
mkdir -p "$T/bin" "$T/vm" "$T/beam-home" "$T/ws-home/.ssh" "$T/ws-config/beams-client" "$T/ws-run"
chmod 700 "$T/ws-run"
: >"$T/ws-config/beams-client/config.toml"

for name in github example other; do
    ssh-keygen -q -t ed25519 -N '' -C '' -f "$T/$name" || exit 1
done
key() { cut -d' ' -f1,2 "$T/$1.pub"; }
{
    echo "github.com $(key github)"
    echo "[git.example.com]:2222 $(key example)"
    echo "@revoked github.com $(key other)"
} >"$T/ws-home/.ssh/known_hosts"
echo "hashed.example $(key other)" >"$T/kh.hashed"
ssh-keygen -q -H -f "$T/kh.hashed" >/dev/null 2>&1 || exit 1
cat "$T/kh.hashed" >>"$T/ws-home/.ssh/known_hosts"
check "the hashed entry hides its name" sh -c "! grep -q hashed.example '$T/ws-home/.ssh/known_hosts'"

cat >"$T/bin/tsh" <<EOF
#!/bin/sh
case " \$* " in
    *"beam-bootstrap client-bridge"*) exec "$B" client-bridge --socket "$VM_SOCK" ;;
esac
echo "fake tsh: unexpected: \$*" >&2
exit 1
EOF
chmod 700 "$T/bin/tsh"

eventually() { local i; for i in $(seq 100); do "$@" && return 0; sleep 0.1; done; return 1; }
lookup() { HOME="$T/beam-home" BEAMS_SESSION_TOKEN=token-a "$B" client known-hosts --socket "$VM_SOCK" "$1"; }

HOME="$T/beam-home" "$B" daemon --socket "$VM_SOCK" >"$T/vm.log" 2>&1 &
pids+=($!)
eventually test -S "$VM_SOCK" || { fail "Beam daemon started"; exit 1; }

lookup github.com >"$T/none.out" 2>"$T/none.err"
check "without a workstation: exit 0" test $? -eq 0
check "without a workstation: no entries" test ! -s "$T/none.out"
check "without a workstation: one stderr line" grep -q "no managed beams client is connected" "$T/none.err"

env HOME="$T/ws-home" BEAMS_KNOWN_HOSTS_FILES="$T/ws-home/.ssh/known_hosts" XDG_CONFIG_HOME="$T/ws-config" XDG_RUNTIME_DIR="$T/ws-run" PATH="$T/bin:$PATH" SSH_AUTH_SOCK= \
    "$C" daemon --socket "$WS_SOCK" >"$T/ws.log" 2>&1 &
pids+=($!)
eventually test -S "$WS_SOCK" || { fail "workstation daemon started"; exit 1; }
python3 - "$WS_SOCK" <<'EOF' || fail "the bridge connected"
import json, socket, sys, time
sock = socket.socket(socket.AF_UNIX)
sock.connect(sys.argv[1])
r, w = sock.makefile("r"), sock.makefile("w")
def send(v): w.write(json.dumps(v) + "\n"); w.flush()
send({"op": "hello", "protocol": "beams-client-services", "version": 1, "role": "frontend"})
assert json.loads(r.readline())["type"] == "hello"
send({"op": "ensure_bridge", "beam_id": "beam-a", "beam_uuid": "beam-uuid", "login": "beams", "session_token": "token-a"})
deadline = time.time() + 20
while time.time() < deadline:
    e = json.loads(r.readline())
    if e.get("type") == "bridge_status" and e["bridge"]["status"] == "connected":
        sys.exit(0)
sys.exit(1)
EOF

lookup github.com >"$T/github.out" 2>"$T/github.err"
check "github.com: its entry and its @revoked entry" test "$(cat "$T/github.out")" = "github.com $(key github)
@revoked github.com $(key other)"
check "github.com: nothing on stderr" test ! -s "$T/github.err"
check "a [host]:port entry matches only with the port" \
    test "$(lookup '[git.example.com]:2222')" = "[git.example.com]:2222 $(key example)" -a -z "$(lookup git.example.com)"
hashed=$(lookup hashed.example)
check "a hashed entry comes back hashed" sh -c "case '$hashed' in '|1|'*\"$(key other)\") exit 0;; esac; exit 1"
check "an unknown host: nothing" test -z "$(lookup unknown.example 2>&1)"
lookup '-oProxyCommand=x' >"$T/bad.out" 2>"$T/bad.err"
check "a pattern or option-like name is refused" grep -q "not a plain host name" "$T/bad.err"
check "the workstation logged the lookups" grep -q "known-hosts: beam-uuid: github.com: 2 entries" "$T/ws.log"

# The answer is valid known_hosts input.
lookup github.com >"$T/answer.kh"
check "the answer parses as known_hosts" sh -c "ssh-keygen -q -F github.com -f '$T/answer.kh' >/dev/null"

echo "$fails failure(s)"
[ "$fails" -eq 0 ]
