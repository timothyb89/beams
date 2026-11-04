#!/usr/bin/env bash
# End-to-end SSH agent proxy test without a real Beam (see
# SSH_AGENT_PROXY_DESIGN.md). Everything runs locally:
#
#   "Beam":        beam-bootstrap daemon (agent socket next to its UDS)
#   bridge:        a fake `tsh` running the real `beam-bootstrap client-bridge`
#   workstation:   beams daemon + a scripted frontend that approves/rejects,
#                  a real ssh-agent holding the keys, a test config.toml and
#                  a hashed known_hosts
#   target:        an unprivileged sshd on 127.0.0.1 and a bare git repo
#
# Then real ssh/git/ssh-add run "on the Beam" with SSH_AUTH_SOCK pointing at
# the Beam daemon's agent socket. Prints PASS/FAIL per check.
set -uo pipefail

ROOT=$(cd -- "$(dirname -- "$0")/../.." && pwd)
CARGO=${CARGO:-cargo}
W=$(mktemp -d "${TMPDIR:-/tmp}/beams-agent-smoke.XXXXXX")
PORT=${PORT:-2231}
fails=0
pids=()
pass() { echo "PASS $*"; }
fail() { echo "FAIL $*"; fails=$((fails + 1)); }
check() { local name=$1; shift; if "$@"; then pass "$name"; else fail "$name"; fi; }

cleanup() {
    for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null; done
    [ -f "$W/sshd.pid" ] && kill "$(cat "$W/sshd.pid")" 2>/dev/null
    [ -n "${AGENT_PID:-}" ] && kill "$AGENT_PID" 2>/dev/null
    if [ "${KEEP_TEST_DIR:-0}" = 1 ] || [ "$fails" -gt 0 ]; then
        echo "logs kept in $W"
    else
        rm -rf "$W"
    fi
}
trap cleanup EXIT

"$CARGO" build -q --manifest-path "$ROOT/bootstrap/Cargo.toml" || exit 1
"$CARGO" build -q --manifest-path "$ROOT/client/Cargo.toml" || exit 1
BOOTSTRAP="$ROOT/bootstrap/target/debug/beam-bootstrap"
BEAMS="$ROOT/client/target/debug/beams"

mkdir -p "$W/bin" "$W/vm" "$W/home/.ssh" "$W/config/beams-client"

# --- workstation agent with an allowed and a hidden key ----------------------
ssh-agent -a "$W/local-agent.sock" -s >"$W/agent.env"
AGENT_PID=$(sed -n 's/^SSH_AGENT_PID=\([0-9]*\);.*/\1/p' "$W/agent.env")
ssh-keygen -q -t ed25519 -N '' -C smoke-allowed -f "$W/id_allowed"
ssh-keygen -q -t ed25519 -N '' -C smoke-hidden -f "$W/id_hidden"
SSH_AUTH_SOCK="$W/local-agent.sock" ssh-add -q "$W/id_allowed" "$W/id_hidden"
rm -f "$W/id_allowed" "$W/id_hidden"   # only the agent holds them
ALLOWED_FP=$(ssh-keygen -lf "$W/id_allowed.pub" | awk '{print $2}')
HIDDEN_FP=$(ssh-keygen -lf "$W/id_hidden.pub" | awk '{print $2}')
printf '[ssh_agent]\nallowed_keys = ["%s"]\nprompt_timeout = 20\n' "$ALLOWED_FP" >"$W/config/beams-client/config.toml"

# --- target sshd, known (hashed) only to the workstation ---------------------
ssh-keygen -q -t ed25519 -N '' -f "$W/hostkey"
cp "$W/id_allowed.pub" "$W/authorized_keys"
cat >"$W/sshd_config" <<EOF
Port $PORT
ListenAddress 127.0.0.1
HostKey $W/hostkey
PidFile $W/sshd.pid
AuthorizedKeysFile $W/authorized_keys
PasswordAuthentication no
KbdInteractiveAuthentication no
UsePAM no
StrictModes no
EOF
/usr/sbin/sshd -f "$W/sshd_config" -E "$W/sshd.log" || { echo "sshd failed"; cat "$W/sshd.log"; exit 1; }
echo "[127.0.0.1]:$PORT $(cat "$W/hostkey.pub")" >"$W/home/.ssh/known_hosts"
ssh-keygen -q -H -f "$W/home/.ssh/known_hosts" >/dev/null 2>&1 && rm -f "$W/home/.ssh/known_hosts.old"
# The "Beam" side's own known_hosts (ssh needs one to connect at all).
echo "[127.0.0.1]:$PORT $(cat "$W/hostkey.pub")" >"$W/vm/known_hosts"

git init -q --bare "$W/repo.git"
git -C "$W" clone -q "$W/repo.git" seed 2>/dev/null
git -C "$W/seed" -c user.name=s -c user.email=s@example.com commit -q --allow-empty -m seed
git -C "$W/seed" push -q origin HEAD:main 2>/dev/null
git -C "$W/repo.git" symbolic-ref HEAD refs/heads/main

# --- fake tsh: only the bridge is needed ---------------------------------------
cat >"$W/bin/tsh" <<EOF
#!/bin/sh
case " \$* " in
    *"beam-client-bridge"*) exec "$BOOTSTRAP" client-bridge --socket "$W/vm/daemon.sock" ;;
esac
echo "fake tsh: unexpected \$*" >&2
exit 1
EOF
chmod 700 "$W/bin/tsh"

# --- scripted frontend ---------------------------------------------------------
cat >"$W/frontend.py" <<'PY'
import json, socket, sys, time
socket_path, decision, sock_for_agent, log_path = sys.argv[1:5]
s = socket.socket(socket.AF_UNIX)
for _ in range(200):
    try:
        s.connect(socket_path); break
    except OSError:
        time.sleep(0.05)
r, w = s.makefile("r"), s.makefile("w")
def send(v): w.write(json.dumps(v) + "\n"); w.flush()
send({"op": "hello", "protocol": "beams-client-services", "version": 1, "role": "frontend"})
send({"op": "ensure_bridge", "beam_id": "smoke", "beam_uuid": "smoke-uuid", "login": "beams",
      "session_token": "smoke-token", "ssh_auth_sock": sock_for_agent})
log = open(log_path, "a")
for line in r:
    event = json.loads(line)
    if event.get("type") == "bridge_status" and event["bridge"]["status"] == "connected":
        print("connected", flush=True)
    if event.get("type") != "request_changed":
        continue
    req = event["request"]
    if req.get("kind") != "ssh_agent":
        continue
    log.write(json.dumps(req, separators=(",", ":")) + "\n"); log.flush()
    if req["status"] == "pending_approval" and decision != "ignore":
        send({"op": "decide_request", "request_id": req["request_id"], "approve": decision == "approve"})
PY

start_frontend() { # decision
    python3 "$W/frontend.py" "$W/local.sock" "$1" "$W/local-agent.sock" "$W/prompts.log" >"$W/frontend-$1.out" 2>&1 &
    FRONTEND_PID=$!
    pids+=("$FRONTEND_PID")
    for _ in $(seq 100); do grep -q connected "$W/frontend-$1.out" 2>/dev/null && break; sleep 0.1; done
    sleep 0.5  # let the key push land
}
stop_frontend() { kill "$FRONTEND_PID" 2>/dev/null; wait "$FRONTEND_PID" 2>/dev/null; sleep 0.5; }

"$BOOTSTRAP" daemon --socket "$W/vm/daemon.sock" >"$W/vm.log" 2>&1 &
pids+=($!)
for _ in $(seq 100); do [ -S "$W/vm/ssh-agent.sock" ] && break; sleep 0.1; done
check "Beam daemon serves an agent socket" test -S "$W/vm/ssh-agent.sock"

env PATH="$W/bin:$PATH" HOME="$W/home" XDG_CONFIG_HOME="$W/config" \
    "$BEAMS" daemon --socket "$W/local.sock" >"$W/local.log" 2>&1 &
pids+=($!)

# --- "on the Beam" ---------------------------------------------------------------
export SSH_AUTH_SOCK="$W/vm/ssh-agent.sock" BEAMS_SESSION_TOKEN=smoke-token
SSHOPTS="-F /dev/null -o UserKnownHostsFile=$W/vm/known_hosts -o StrictHostKeyChecking=yes -o BatchMode=yes -o IdentityFile=none -p $PORT"
ME=$(id -un)

check "no keys listed before a frontend attaches" bash -c "[ \"\$(ssh-add -l 2>&1)\" = 'The agent has no identities.' ]"

start_frontend approve
listed=$(ssh-add -l 2>&1)
check "frontend attached: only the allowed key is listed" \
    bash -c "grep -qF '$ALLOWED_FP' <<<'$listed' && ! grep -qF '$HIDDEN_FP' <<<'$listed'"
check "key management is refused" bash -c "! ssh-add -D 2>/dev/null"

SSH_KEY_REASON="smoke: plain login" ssh $SSHOPTS "$ME@127.0.0.1" true
check "approved ssh login succeeds" test $? -eq 0
check "prompt named the verified host via hashed known_hosts" \
    grep -q '"host":{"status":"verified","name":"\[127.0.0.1\]:'"$PORT"'"}' "$W/prompts.log"
check "prompt carried the claimed reason" grep -q '"reason":"smoke: plain login"' "$W/prompts.log"
check "request ended signed" grep -q '"status":"running","request":null,"action":null,"message":"signed"' "$W/prompts.log"

GIT_SSH_COMMAND="ssh $SSHOPTS" SSH_KEY_REASON="smoke: clone" \
    git clone -q "ssh://$ME@127.0.0.1:$PORT$W/repo.git" "$W/clone"
check "approved git clone succeeds" test -d "$W/clone/.git"
check "clone prompt summarized the ssh command" grep -q "\"command\":\"ssh $ME@127.0.0.1 git-upload-pack" "$W/prompts.log"

echo smoke >"$W/msg"
ssh-keygen -q -Y sign -n file -f "$W/id_allowed.pub" "$W/msg" </dev/null 2>/dev/null
check "approved ssh-keygen -Y sign succeeds" test -s "$W/msg.sig"
check "signature prompt named its namespace" grep -q '"operation":{"kind":"signature","namespace":"file"}' "$W/prompts.log"

stop_frontend
start_frontend reject
ssh $SSHOPTS "$ME@127.0.0.1" true 2>/dev/null
check "rejected login fails" test $? -ne 0
check "rejection recorded" grep -q '"status":"rejected"' "$W/prompts.log"

stop_frontend
start_frontend ignore
start=$(date +%s)
ssh $SSHOPTS "$ME@127.0.0.1" true 2>/dev/null
rc=$?
took=$(( $(date +%s) - start ))
check "unanswered login fails after the prompt timeout" test $rc -ne 0 -a $took -ge 19 -a $took -le 40
echo "     (${took}s)"
check "expiry recorded" grep -q '"status":"expired"' "$W/prompts.log"

stop_frontend
check "frontend detached: keys withdrawn" bash -c "[ \"\$(ssh-add -l 2>&1)\" = 'The agent has no identities.' ]"
start=$(date +%s)
ssh $SSHOPTS "$ME@127.0.0.1" true 2>/dev/null
check "login with no frontend fails without waiting" test $? -ne 0 -a $(( $(date +%s) - start )) -lt 5

echo
grep -h 'ssh-agent' "$W/local.log" | sed 's/^/LOG /' | head -12
echo "$fails failure(s)"
exit "$fails"
