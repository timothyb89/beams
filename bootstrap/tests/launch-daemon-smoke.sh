#!/usr/bin/env bash
# End-to-end check of daemon-accepted launches (docs/development/launches.md)
# with the real binaries and nothing outside a scratch directory: its own
# HOME, XDG config, daemon socket and launch state. A tiny profile with a pre
# hook, a post file and a post hook (no packages, so no apt) is packed by the
# client, submitted before anything is staged, then released step by step.
# Never touches this machine's own daemon. Prints PASS/FAIL per check.
set -uo pipefail

ROOT=$(cd -- "$(dirname -- "$0")/../.." && pwd)
CARGO=${CARGO:-cargo}
S=$(mktemp -d "${TMPDIR:-/tmp}/beams-launch-smoke.XXXXXX")
fails=0
pids=()
pass() { echo "PASS $*"; }
fail() { echo "FAIL $*"; fails=$((fails + 1)); }
check() { local name=$1; shift; if "$@"; then pass "$name"; else fail "$name"; fi; }
cleanup() {
    for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null; done
    # The TUI and its `script` (matched by this run's unique socket path).
    pkill -9 -f "$SOCK" 2>/dev/null
    if [ "${KEEP_TEST_DIR:-0}" = 1 ] || [ "$fails" -gt 0 ]; then echo "kept $S"; else chmod -R u+w "$S"; rm -rf "$S"; fi
}
trap cleanup EXIT

"$CARGO" build -q --manifest-path "$ROOT/bootstrap/Cargo.toml" || exit 1
"$CARGO" build -q --manifest-path "$ROOT/client/Cargo.toml" || exit 1
B="$ROOT/bootstrap/target/debug/beam-bootstrap"
C="$ROOT/client/target/debug/beams"
export HOME="$S/home" XDG_CONFIG_HOME="$S/xdg"
unset SSH_AUTH_SOCK
SOCK="$S/run/daemon.sock"
mkdir -p "$HOME" "$XDG_CONFIG_HOME" "$S/profile/hooks" "$S/profile/data"

cat >"$S/profile/environment.toml" <<'EOF'
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
printf 'echo preparing; echo pre >> "$HOME/events"\n' >"$S/profile/hooks/prepare.sh"
printf 'cat "$HOME/smoke-data/input" >> "$HOME/events"\n' >"$S/profile/hooks/configure.sh"
echo post >"$S/profile/data/input"

events() { tr '\n' ' ' <"$HOME/events" 2>/dev/null; }
status_has() { "$B" launch status --socket "$SOCK" | grep -Eq "$1"; }
unpack() {
    "$B" launch unpack --archive "$S/launch/$1/bundle.tar.gz" \
        --descriptor "$S/launch/$1/descriptor.json" --output "$S/stage/$1" >/dev/null
}
signal() { "$B" launch signal --socket "$SOCK" --launch-id "$ID" --condition "$1" >/dev/null 2>&1; }
eventually() { local i; for i in $(seq 50); do "$@" && return 0; sleep 0.1; done; return 1; }

"$C" config pack --profile "$S/profile/environment.toml" --output "$S/launch" >/dev/null || exit 1
ID=$(sed -n 's/.*"launch_id": "\(.*\)".*/\1/p' "$S/launch/launch.json" | head -1)
"$B" daemon --socket "$SOCK" >"$S/daemon.log" 2>&1 &
pids+=($!)
eventually test -S "$SOCK" || { fail "daemon started"; exit 1; }

"$B" launch submit --socket "$SOCK" --launch "$S/launch/launch.json" --staging "$S/stage" >"$S/follow.log" 2>&1 &
follower=$!
check "an unstaged launch waits for its pre bundle" eventually status_has 'waiting +pre +Verified pre archive'
check "nothing ran before its bundle" test ! -e "$HOME/events"
check "a signal with nothing staged is refused" bash -c "! '$B' launch signal --socket '$SOCK' --launch-id '$ID' --condition artifact-ready:pre >/dev/null 2>&1"

unpack pre
check "the pre bundle is released by signal" signal artifact-ready:pre
check "pre runs, then post waits for the interface" eventually status_has 'waiting +post +Shell and first TUI frame ready'
check "pre hook ran once" test "$(events)" = "pre "
check "post work is still pending" status_has 'pending +post +Configure'

"$C" config pack-post --launch-dir "$S/launch" >/dev/null
unpack post
check "launch ready is a no-op outside a launch" "$B" launch ready --socket "$SOCK" --component shell
check "the shell reports ready" env BEAMS_LAUNCH_ID="$ID" "$B" launch ready --socket "$SOCK" --component shell
check "the interface then waits only for the TUI" eventually status_has 'waiting +post +Shell and first TUI frame ready - waiting for the TUI to start'
if command -v script >/dev/null; then
    # The real TUI, on a pseudo-terminal, reports once its launch's
    # snapshot is on screen.
    BEAMS_LAUNCH_ID="$ID" TERM=xterm script -qc "'$B' tui --socket '$SOCK'" /dev/null \
        < <(sleep 60) >/dev/null 2>&1 &
    pids+=($!)
    disown "$!"
    check "the TUI reports ready" eventually status_has 'ready: \[shell, tui\]'
else
    check "the TUI reports ready" "$B" launch ready --socket "$SOCK" --component tui --launch-id "$ID"
fi
check "interface-ready is released by both" eventually status_has 'released: \[.*interface-ready'
check "the post bundle is released by signal" signal artifact-ready:post
wait "$follower"
check "the follower exits 0 when the launch succeeds" test $? -eq 0
check "post ran after pre" test "$(events)" = "pre post "
check "the follower printed the hook's output" grep -q 'stdout: preparing' "$S/follow.log"
check "the follower reported completion" grep -q '^launch Ok' "$S/follow.log"
check "a resubmission is the same launch" bash -c "'$B' launch submit --socket '$SOCK' --launch '$S/launch/launch.json' --staging '$S/stage' | grep -q 'already accepted'"
check "a finished launch can't be retried" bash -c "! '$B' launch retry --socket '$SOCK' --launch-id '$ID' 2>/dev/null"

# A restarted daemon reloads the accepted launch.
kill "${pids[0]}"; wait "${pids[0]}" 2>/dev/null
"$B" daemon --socket "$SOCK" >>"$S/daemon.log" 2>&1 &
pids=($!)
eventually test -S "$SOCK"
check "a restarted daemon still knows the launch" eventually status_has "$ID Ok"

echo "$fails failure(s)"
exit "$fails"
