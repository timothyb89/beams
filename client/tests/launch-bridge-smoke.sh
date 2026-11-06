#!/usr/bin/env bash
# End-to-end `beams config launch` without a real Beam: the real workstation
# daemon and `beams` CLI on one side, the real `beam-bootstrap` daemon on the
# other, and a fake `tsh` that runs `beam-bootstrap client-bridge` locally
# (the exact stdio bridge protocol) and lists one Beam. A tiny profile (a pre
# hook, a few MiB of post data, a post hook) is packed, uploaded in chunks over
# the bridge, staged by the Beam and run: once headless, once released by
# shell/TUI readiness. Everything lives in a scratch directory; this
# machine's own daemons are never touched. Prints PASS/FAIL per check.
set -uo pipefail

ROOT=$(cd -- "$(dirname -- "$0")/../.." && pwd)
CARGO=${CARGO:-cargo}
T=$(mktemp -d "${TMPDIR:-/tmp}/beams-launch-bridge.XXXXXX")
fails=0
pids=()
pass() { echo "PASS $*"; }
fail() { echo "FAIL $*"; fails=$((fails + 1)); }
check() { local name=$1; shift; if "$@"; then pass "$name"; else fail "$name"; fi; }
cleanup() {
    for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null; done
    # The workstation daemon `beams` spawned, and the bridge (matched by
    # this run's unique scratch path).
    pkill -f "$T/" 2>/dev/null
    if [ "${KEEP_TEST_DIR:-0}" = 1 ] || [ "$fails" -gt 0 ]; then
        echo "kept $T"
    else
        chmod -R u+w "$T"
        rm -rf "$T"
    fi
}
trap cleanup EXIT

"$CARGO" build -q --manifest-path "$ROOT/bootstrap/Cargo.toml" || exit 1
"$CARGO" build -q --manifest-path "$ROOT/client/Cargo.toml" || exit 1
B="$ROOT/bootstrap/target/debug/beam-bootstrap"
C="$ROOT/client/target/debug/beams"
VM_SOCK="$T/vm/daemon.sock"
BEAM_HOME="$T/beam-home"
mkdir -p "$T/bin" "$T/vm" "$BEAM_HOME" "$T/ws-home" "$T/ws-config" "$T/ws-run" "$T/profile/hooks" "$T/profile/data"
chmod 700 "$T/ws-run"
# No startup manifest: the default one installs toolchains into the scratch
# Beam home (over the network). Launches are all this test exercises.
mkdir -p "$T/ws-config/beams-client"
printf '[startup]\ntasks = []\n' >"$T/ws-config/beams-client/config.toml"

cat >"$T/bin/tsh" <<EOF
#!/bin/sh
case " \$* " in
    *" beams ls "*)
        echo '[{"id": "beam-a", "uuid": "beam-uuid", "owner": "me", "expires": "2099-01-01T00:00:00Z"}]'
        exit 0 ;;
    *"beam-client-bridge"*)
        exec "$B" client-bridge --socket "$VM_SOCK" ;;
esac
echo "fake tsh: unexpected: \$*" >&2
exit 1
EOF
chmod 700 "$T/bin/tsh"

cat >"$T/profile/environment.toml" <<'EOF'
schema_version = 1
extends = ["builtin:core"]

[files.data]
source = "./data"
target = "~/smoke-data"
phase = "post"

[tasks.prepare]
label = "Prepare"
script = "./hooks/prepare.sh"
phase = "pre"

[tasks.configure]
label = "Configure"
script = "./hooks/configure.sh"
after = ["files:data"]
EOF
printf 'echo pre >> "$HOME/events"\n' >"$T/profile/hooks/prepare.sh"
printf 'echo post >> "$HOME/events"\n' >"$T/profile/hooks/configure.sh"
# Incompressible, so the post bundle spans many chunks and windows.
head -c $((3 * 1024 * 1024)) /dev/urandom >"$T/profile/data/blob"

HOME="$BEAM_HOME" "$B" daemon --socket "$VM_SOCK" >"$T/vm.log" 2>&1 &
pids+=($!)
eventually() { local i; for i in $(seq 100); do "$@" && return 0; sleep 0.1; done; return 1; }
eventually test -S "$VM_SOCK" || { fail "Beam daemon started"; exit 1; }

ws() {
    env HOME="$T/ws-home" XDG_CONFIG_HOME="$T/ws-config" XDG_RUNTIME_DIR="$T/ws-run" \
        PATH="$T/bin:$PATH" SSH_AUTH_SOCK= "$@"
}
events() { tr '\n' ' ' <"$BEAM_HOME/events" 2>/dev/null; }
beam_status() { HOME="$BEAM_HOME" "$B" launch status --socket "$VM_SOCK" --launch-id "$1" 2>/dev/null; }

# Headless: everything runs once the bundles arrive.
ws timeout 120 "$C" config launch --profile "$T/profile/environment.toml" --headless beam-a >"$T/headless.log" 2>&1
check "a headless launch over the bridge succeeds" test $? -eq 0
ID=$(sed -n 's/^beams: launch \([0-9a-f-]*\) .*/\1/p' "$T/headless.log" | head -1)
check "the launch ID was reported" test -n "$ID"
check "pre ran, then post" test "$(events)" = "pre post "
check "the post data arrived intact" cmp -s "$T/profile/data/blob" "$BEAM_HOME/smoke-data/blob"
check "upload progress was shown" grep -q 'post [0-9]*/[0-9]* bytes' "$T/headless.log"
check "the Beam staged both bundles itself" test -d "$T/vm/launch-state/$ID/staging/pre" -a -d "$T/vm/launch-state/$ID/staging/post"
check "consumed uploads were removed" test ! -e "$T/vm/launch-uploads/$ID"
check "the workstation recorded the outcome" grep -q "\"$ID\"" "$T/ws-config/beams-client/daemon-state.json"

# Interactive: post waits for the shell and TUI to report ready.
rm -f "$BEAM_HOME/events"
ws timeout 120 "$C" config launch --profile "$T/profile/environment.toml" beam-a >"$T/interactive.log" 2>&1 &
follower=$!
eventually grep -q '^beams: launch ' "$T/interactive.log"
ID2=$(sed -n 's/^beams: launch \([0-9a-f-]*\) .*/\1/p' "$T/interactive.log" | head -1)
check "an interactive launch waits for the shell and TUI" eventually bash -c "$(declare -f beam_status); B='$B' BEAM_HOME='$BEAM_HOME' VM_SOCK='$VM_SOCK'; beam_status '$ID2' | grep -q 'waiting for the shell and TUI'"
check "pre ran before the interface" test "$(events)" = "pre "
HOME="$BEAM_HOME" BEAMS_LAUNCH_ID="$ID2" "$B" launch ready --socket "$VM_SOCK" --component shell
HOME="$BEAM_HOME" BEAMS_LAUNCH_ID="$ID2" "$B" launch ready --socket "$VM_SOCK" --component tui
wait "$follower"
check "readiness released post, and the launch succeeded" test $? -eq 0
check "post ran after the interface" test "$(events)" = "pre post "
check "no startup manifest ran" test ! -e "$BEAM_HOME/.rustup" -a ! -e "$BEAM_HOME/.cargo"

echo "$fails failure(s)"
exit "$fails"
