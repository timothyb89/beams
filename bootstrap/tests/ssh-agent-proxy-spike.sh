#!/usr/bin/env bash
# Phase 0 spike for docs/designs/SSH_AGENT_PROXY_DESIGN.md. Run on a disposable Beam:
#
#   BIN=/path/to/beam-bootstrap bootstrap/tests/ssh-agent-proxy-spike.sh
#
# Builds throwaway fixtures under a temp dir (an upstream ssh-agent holding
# two generated keys, an unprivileged sshd on 127.0.0.1:2200 with a *hashed*
# known_hosts entry, a bare git repo), runs `beam-bootstrap
# agent-proxy-spike` in front of the agent, and drives ssh/git/ssh-keygen
# through it. Prints PASS/FAIL per check; the proxy's JSON log is kept at
# $W/proxy*.log for inspection.
#
# Optional: GITHUB=1 also checks session-bind against github.com (the
# generated key is rejected there, which is the point: no real key needed);
# GITHUB_GRACE=1 additionally probes how long github.com waits pre-auth.
# LONG_DELAY_MS (default 45000) is the simulated approval delay for the
# slow-approval checks.
set -uo pipefail

BIN=${BIN:?set BIN to a beam-bootstrap binary}
LONG_DELAY_MS=${LONG_DELAY_MS:-45000}
W=$(mktemp -d /tmp/agent-spike.XXXXXX)
PORT=2200
fails=0
pass() { echo "PASS $*"; }
fail() { echo "FAIL $*"; fails=$((fails + 1)); }
check() { local name=$1; shift; if "$@"; then pass "$name"; else fail "$name"; fi; }
log_has() { grep -q -- "$1" "$PROXY_LOG"; }

cleanup() {
    [ -n "${PROXY_PID:-}" ] && kill "$PROXY_PID" 2>/dev/null
    [ -f "$W/sshd.pid" ] && kill "$(cat "$W/sshd.pid")" 2>/dev/null
    [ -n "${UPSTREAM_PID:-}" ] && kill "$UPSTREAM_PID" 2>/dev/null
    echo "fixtures and logs left in $W"
}
trap cleanup EXIT

# --- fixtures ---------------------------------------------------------------
ssh-agent -a "$W/upstream.sock" -s >"$W/upstream.env"
UPSTREAM_PID=$(sed -n 's/^SSH_AGENT_PID=\([0-9]*\);.*/\1/p' "$W/upstream.env")
ssh-keygen -q -t ed25519 -N '' -C spike-allowed -f "$W/id_allowed"
ssh-keygen -q -t ed25519 -N '' -C spike-hidden -f "$W/id_hidden"
SSH_AUTH_SOCK="$W/upstream.sock" ssh-add -q "$W/id_allowed" "$W/id_hidden"
# Only the agent holds the private keys from here on: otherwise ssh-keygen
# (and ssh) silently fall back to the file next to the .pub.
rm -f "$W/id_allowed" "$W/id_hidden"
ALLOWED_FP=$(ssh-keygen -lf "$W/id_allowed.pub" | awk '{print $2}')
HIDDEN_FP=$(ssh-keygen -lf "$W/id_hidden.pub" | awk '{print $2}')

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
LoginGraceTime 120
EOF
/usr/sbin/sshd -f "$W/sshd_config" -E "$W/sshd.log" || { echo "sshd failed to start"; cat "$W/sshd.log"; exit 1; }

# Hashed known_hosts (Debian's HashKnownHosts default) for the local sshd.
echo "[localhost]:$PORT $(cat "$W/hostkey.pub")" >"$W/known_hosts"
ssh-keygen -q -H -f "$W/known_hosts" >/dev/null 2>&1 && rm -f "$W/known_hosts.old"
if [ -n "${GITHUB:-}" ]; then
    ssh-keyscan -q github.com >>"$W/known_hosts" 2>/dev/null
fi

git init -q --bare "$W/repo.git"
git -C "$W" clone -q "$W/repo.git" seed 2>/dev/null
git -C "$W/seed" -c user.name=spike -c user.email=spike@example.com commit -q --allow-empty -m seed
git -C "$W/seed" push -q origin HEAD:main 2>/dev/null
git -C "$W/repo.git" symbolic-ref HEAD refs/heads/main

SSHOPTS="-F /dev/null -o UserKnownHostsFile=$W/known_hosts -o StrictHostKeyChecking=yes -o BatchMode=yes -o IdentityFile=none"
ME=$(id -un)

start_proxy() { # name delay_ms [delay_identities_ms]
    [ -n "${PROXY_PID:-}" ] && kill "$PROXY_PID" 2>/dev/null && wait "$PROXY_PID" 2>/dev/null
    PROXY_LOG="$W/proxy-$1.log"
    "$BIN" agent-proxy-spike --listen "$W/proxy.sock" --upstream "$W/upstream.sock" \
        --known-hosts "$W/known_hosts" --allow "$ALLOWED_FP" \
        --delay-ms "$2" --delay-identities-ms "${3:-0}" >"$PROXY_LOG" 2>&1 &
    PROXY_PID=$!
    for _ in $(seq 50); do [ -S "$W/proxy.sock" ] && break; sleep 0.1; done
}
export SSH_AUTH_SOCK="$W/proxy.sock"

# --- basic filtering --------------------------------------------------------
start_proxy basic 0
listed=$(ssh-add -l 2>&1)
check "list exposes only the allowlisted key" bash -c "grep -qF '$ALLOWED_FP' <<<'$listed' && ! grep -qF '$HIDDEN_FP' <<<'$listed'"
check "ssh-add (add identity) is refused" bash -c "! ssh-add -q '$W/id_hidden' 2>/dev/null"
check "ssh-add -D (remove all) is refused" bash -c "! ssh-add -D 2>/dev/null"
check "ssh-add -x (lock) is refused" bash -c "! echo pw | SSH_ASKPASS_REQUIRE=never ssh-add -x </dev/null 2>/dev/null"
SSH_AUTH_SOCK="$W/upstream.sock" check "upstream still holds both keys" bash -c "[ \$(ssh-add -l | wc -l) -eq 2 ]"

# --- ssh login: session-bind + hostbound + hashed known_hosts ----------------
SSH_KEY_REASON=$'Spike: plain ssh login \e[31m(with an escape)' \
    ssh $SSHOPTS -p $PORT "$ME@localhost" true
check "ssh login through proxy succeeds" test $? -eq 0
check "session-bind verified" log_has '"event":"session_bind".*"verified":true'
check "host named via hashed known_hosts" log_has '"verified":"\[localhost\]:2200","via":"hashed known_hosts"'
check "auth payload parsed with user" log_has "\"kind\":\"auth\".*\"user\":\"$ME\""
check "SSH_KEY_REASON recovered and sanitized" log_has '"reason_claimed":"Spike: plain ssh login \[31m(with an escape)"'

# --- git over ssh, with reason + task attribution ---------------------------
GIT_SSH_COMMAND="ssh $SSHOPTS" BEAMS_TASK_RUN="manifest-1/clone-git-repo:spike" \
    SSH_KEY_REASON="Spike: clone for review" \
    git clone -q "ssh://$ME@localhost:$PORT$W/repo.git" "$W/clone"
check "git clone over ssh succeeds" test -d "$W/clone/.git"
check "task attribution recovered from ancestor env" log_has '"task_run_claimed":"manifest-1/clone-git-repo:spike"'
check "git-upload-pack visible in chain argv" log_has 'git-upload-pack'

# --- ProxyJump: two ssh processes, each binding its own session ------------
ssh $SSHOPTS -o "ProxyCommand=ssh $SSHOPTS -p $PORT -W %h:%p $ME@localhost" -p $PORT "$ME@localhost" true
check "ProxyJump-style nested login succeeds" test $? -eq 0
check "nested login: >=4 verified binds total" bash -c "[ \$(grep -c '\"event\":\"session_bind\".*\"verified\":true' '$PROXY_LOG') -ge 4 ]"

# --- SSHSIG: ssh-keygen -Y sign and git commit signing ----------------------
echo hello >"$W/msg"
ssh-keygen -q -Y sign -n file -f "$W/id_allowed.pub" "$W/msg" </dev/null 2>/dev/null
check "ssh-keygen -Y sign via agent" test -s "$W/msg.sig"
check "sshsig namespace 'file' parsed" log_has '"kind":"sshsig","namespace":"file"'
git -C "$W/clone" -c user.name=spike -c user.email=spike@example.com -c gpg.format=ssh \
    -c user.signingkey="$W/id_allowed.pub" commit -q --allow-empty -S -m signed
check "git commit -S via agent" test $? -eq 0
check "sshsig namespace 'git' parsed" log_has '"kind":"sshsig","namespace":"git"'

# --- refused key / unknown payload ------------------------------------------
cp "$W/msg" "$W/msg2"
check "sign with a non-allowlisted key is denied" bash -c \
    "! ssh-keygen -q -Y sign -n file -f '$W/id_hidden.pub' '$W/msg2' </dev/null 2>/dev/null"

# --- concurrency -------------------------------------------------------------
before=$(grep -c '"signed":true' "$PROXY_LOG")
pids=()
for i in 1 2 3 4; do ssh $SSHOPTS -p $PORT "$ME@localhost" true & pids+=($!); done
wait "${pids[@]}"  # not a bare `wait`: that would also wait for the proxy
after=$(grep -c '"signed":true' "$PROXY_LOG")
check "4 parallel logins all signed" test $((after - before)) -eq 4

# --- slow approval -----------------------------------------------------------
start_proxy slow "$LONG_DELAY_MS"
start=$(date +%s)
GIT_SSH_COMMAND="ssh $SSHOPTS" git ls-remote -q "ssh://$ME@localhost:$PORT$W/repo.git" >/dev/null
rc=$?
check "git ls-remote survives a ${LONG_DELAY_MS}ms approval delay" test $rc -eq 0
echo "     (took $(( $(date +%s) - start ))s)"

# Beyond the server's LoginGraceTime the server hangs up first.
# (sshd keeps the *first* value of a keyword, so replace rather than append.)
sed -i 's/^LoginGraceTime .*/LoginGraceTime 10/' "$W/sshd_config"
kill "$(cat "$W/sshd.pid")"; sleep 0.5
/usr/sbin/sshd -f "$W/sshd_config" -E "$W/sshd.log"
start_proxy grace 15000
ssh $SSHOPTS -p $PORT "$ME@localhost" true 2>"$W/grace.err"
check "approval slower than LoginGraceTime fails" test $? -ne 0
echo "     ($(tr '\n' ' ' <"$W/grace.err" | cut -c1-160))"

# --- github.com --------------------------------------------------------------
if [ -n "${GITHUB:-}" ]; then
    start_proxy github 0
    ssh $SSHOPTS -T git@github.com 2>"$W/github.err"
    check "github.com session-bind verified and named" log_has '"event":"session_bind".*"known_hosts":\[{"hashed":false,"name":"github.com"}\].*"verified":true'
    check "github.com rejects the throwaway key without a sign request" bash -c "! grep -q '\"event\":\"sign_request\"' '$PROXY_LOG'"
    # How long will GitHub wait for us before auth? Probe with slow listings
    # (GITHUB_GRACE=1; takes ~3.5 minutes).
    for d in ${GITHUB_GRACE:+20 60 120}; do
        start_proxy "github-grace-$d" 0 $((d * 1000))
        start=$(date +%s)
        ssh $SSHOPTS -T git@github.com 2>"$W/github-grace-$d.err"
        took=$(( $(date +%s) - start ))
        if grep -q 'Permission denied' "$W/github-grace-$d.err"; then
            echo "INFO github.com still talking after ${d}s pre-auth delay (${took}s total)"
        else
            echo "INFO github.com dropped us with a ${d}s pre-auth delay after ${took}s: $(head -c 160 "$W/github-grace-$d.err")"
        fi
    done
fi

# --- tsh with the proxy as SSH_AUTH_SOCK ---------------------------------------
if command -v tsh >/dev/null; then
    start_proxy tsh 0
    tsh status >/dev/null 2>&1
    tsh ls >/dev/null 2>&1
    if grep -q '"event":"refused"' "$PROXY_LOG"; then
        echo "INFO tsh touched the agent: $(grep '"event":"refused"' "$PROXY_LOG" | head -3 | tr '\n' ' ')"
    else
        echo "INFO tsh status/ls did not try to modify the agent"
    fi
    grep -c '"event":"accept"' "$PROXY_LOG" | sed 's/^/INFO tsh agent connections: /'
fi

echo
grep -h '"event":"accept"' "$W"/proxy-basic.log | head -1 | sed 's/^/SAMPLE accept: /'
grep -h '"event":"sign_request"' "$W"/proxy-basic.log | head -1 | sed 's/^/SAMPLE sign: /'
echo "gather times (us): $(grep -ho '"gather_us":[0-9]*' "$W"/proxy-*.log | cut -d: -f2 | sort -n | tr '\n' ' ')"
echo "$fails failure(s)"
exit "$fails"
