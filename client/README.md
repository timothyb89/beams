# beams (client wrapper)

Runs on your local workstation (not inside a beam). Spawns or reuses a
Teleport Beam and drops you straight into it, per project.md's "secondarily"
section. Shells out directly to `tsh beams ls/add/scp` and `tsh ssh`, always
using your current `tsh` identity - no admin credentials, no dependency on
`~/teleport-client-rs` (see commit history for why: its reusable surface,
`TshExecutor`, only covers 2 of the ~5 operations needed here, and would
pull in a heavy gRPC/TLS dependency tree for that).

## `beams ssh`

```sh
beams ssh [--repo ~/beams] [--purpose default] [--region REGION] [--login beams]
```

1. Looks up the beam tracked for `--purpose` (see "State" below). If it
   exists and has more than 12 hours of TTL left (beams can't have their
   TTL extended), reuses it - no file copy, straight to connecting.
2. Otherwise spawns a new beam (`tsh beams add`), refusing if already at
   the 5-concurrent-beam limit (`tsh beams rm` something first).
3. Pushes `--repo`'s current branch onto the beam over git's `ext::`
   transport, run through the same `tsh ssh` channel used everywhere
   else here - no new credential, not the repo's own dedicated ssh
   keypair (see `DESIGN.md`'s "Repo delivery onto a beam" for the full
   design, including a real gotcha found verifying this against a live
   beam: the beam-side repo's checked-out branch has to match what gets
   pushed, or `receive.denyCurrentBranch=updateInstead` silently no-ops
   instead of erroring). Only transfers committed history, like any git
   push. Also points the beam's own `origin` remote at `--repo`'s real
   `origin` (plain GitHub URL, not the `ext::` one used for this push,
   which only works from this workstation) - without this the beam ends
   up with no `origin` configured at all, since neither `git init` nor
   a push over a bare URL creates one. Skipped entirely if `--repo`
   itself has no `origin` configured.
4. Separately `scp`s the two things deliberately *not* tracked in git
   but still required - the ssh private key, and a prebuilt
   `beam-bootstrap` binary (**build this first**: beams run Linux, so if
   you're on macOS (or just want a statically linked build) run
   `../bootstrap/build-cross.sh`; on Linux, `cd ../bootstrap && cargo
   build --release` also works. No Rust toolchain on a fresh beam, so the
   binary has to be copied over either way - see `../bootstrap/README.md`.
   `stage.rs` looks for a cross-compiled build first and falls back to a
   native one, refusing to ship anything that isn't a Linux ELF binary.)
5. Registers a random managed-session token with the local daemon's
   per-Beam bridge, then connects with `tsh ssh -t <login>@<uuid> -- env
   BEAMS_CLIENT_ID=... BEAMS_SESSION_TOKEN=... ~/beams/bin/beam-shell` -
   bypassing the `tsh beams ssh` layer (which can't take a remote command)
   in favor of plain `tsh ssh`, per project.md. Unlike the original direct
   `exec` handoff, `beams` now supervises `tsh` under a local PTY: terminal
   bytes, resize events, exit status, and signals still pass through, while
   `Ctrl-]` can temporarily open a local menu over the remote session.
   `beam-shell` unlocks the ssh-agent key, kicks off `beam-bootstrap` in the
   background, and drops you into zellij with a log pane - see
   `../bootstrap/README.md`.

On reuse, steps 3-4 are skipped entirely - a reused beam is assumed to
already be bootstrapped from a prior run.

## Managed SSH session and local menu

During `beams ssh`, press `Ctrl-]` to open the workstation-side Beams menu.
Press `q`, escape, or enter to close it; press `Ctrl-]` a second time to close
the menu and send a literal `Ctrl-]` byte to the remote terminal. Configure the
prefix with `--escape-key`:

```sh
beams ssh --escape-key ctrl-g
beams ssh --escape-key 0x1d
beams ssh --escape-key none   # pure passthrough; local menu disabled
```

The menu shows connection/bridge state and typed remote requests. A pending
port-forward proposal displays its requested local port (or automatic
selection) and Beam endpoint; `y` approves and `n` rejects it. Phase 3 only
round-trips that decision—approval does not create a listener until the Phase
4 forwarding implementation lands.

The client does not nest another terminal alternate screen over remote zellij
(alternate-screen state is not stackable). It drains up to 1 MiB of remote
output while the menu is visible, restores that output on close, and briefly
resizes/restores the child PTY to make the remote application redraw. Terminal
raw mode and cursor visibility are restored on normal exit, errors, and handled
termination signals.

## Local service daemon

`beams ssh` connects to a per-user workstation daemon and starts one in the
background if no instance is listening. The foreground SSH process still owns
the interactive PTY. The daemon owns one reconnecting
`tsh ssh ... ~/beams/bin/beam-client-bridge` process per Beam, shares it across
local frontends, and retains all managed-shell tokens when that bridge
reconnects.

Run it explicitly in the foreground for debugging:

```sh
beams daemon
beams daemon --socket /tmp/beams-test.sock
```

`--no-client-daemon` on `beams ssh` disables connection/auto-start without
disabling the managed PTY itself.

The default socket is `$XDG_RUNTIME_DIR/beams-client/daemon.sock` when
`XDG_RUNTIME_DIR` is set, otherwise
`~/.config/beams-client/daemon.sock`. It and the versioned state file
`~/.config/beams-client/daemon-state.json` are mode `0600`; the state file is
replaced atomically and contains the workstation's stable random `client_id`
plus the request/service registry. Auto-start diagnostics go to
`~/.config/beams-client/daemon.log`.

The local protocol is typed, versioned newline-delimited JSON. It requires a
frontend handshake, limits frames to 64 KiB, bounds event queues, rate-limits
each connection, and times out incomplete handshakes. Frontends receive an
initial snapshot and subsequent daemon, bridge, and request broadcasts. If the
local daemon restarts while an interactive session remains open, that frontend
reconnects, re-registers its Beam/token pair, and keeps the remote shell
running. Pending approvals orphaned by a daemon restart become `expired`
rather than being silently revived.

The cross-SSH bridge uses the shared typed protocol in `../protocol/`.
`beamsd` generates a fresh `bridge_id` for each reconnect, sends its
authoritative request/service snapshots, and only then marks the bridge ready.
That synchronization barrier prevents a just-submitted request from being
overwritten by a late restart snapshot. A Beam can submit proposals and
observe decisions, but the bridge protocol has no VM-to-client approval
message.

Inside a managed Beam shell:

```sh
beam-bootstrap client list
beam-bootstrap client request port-forward --remote-port 8080
```

A direct SSH shell has no managed token and receives an actionable error. A
stale token is also rejected rather than falling back to another workstation.
The request expires after 60 seconds if no local frontend decides it.

## State

No server-side way exists to label a beam, so `~/.config/beams-client/state.json`
tracks which beam id serves which `--purpose` locally. `default` is what
plain `beams ssh` reuses; pass `--purpose` to track a separate beam for a
side task (e.g. reviewing a specific PR) without it being mistaken for your
default one. This is a simple heuristic, not a full design - revisit if
purposes outgrow this.

## Building

```sh
cd client
cargo build --release
./target/release/beams ssh
```

Tests:

```sh
cargo test
./tests/managed-session-smoke.sh
./tests/client-bridge-smoke.sh
```

The managed-session smoke test is Unix-specific (`script(1)` + a fake `tsh`
fixture) and
exercises the compiled client inside a real controlling terminal: local menu
rendering, raw input around the prefix, PTY size propagation, deferred remote
output, and child exit-code propagation.

The client-bridge smoke test runs both real daemons and the real
`beam-bootstrap client-bridge`, with only the outer SSH executable replaced by
a local fixture. It covers two independent workstation clients attached to
one Beam, exact token routing, rejection, snapshot synchronization, and both
VM- and workstation-daemon restarts. It needs permission to create temporary
Unix sockets.
