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
   push.
4. Separately `scp`s the two things deliberately *not* tracked in git
   but still required - the ssh private key, and a prebuilt
   `beam-bootstrap` binary (**build this first**: beams run Linux, so if
   you're on macOS (or just want a statically linked build) run
   `../bootstrap/build-cross.sh`; on Linux, `cd ../bootstrap && cargo
   build --release` also works. No Rust toolchain on a fresh beam, so the
   binary has to be copied over either way - see `../bootstrap/README.md`.
   `stage.rs` looks for a cross-compiled build first and falls back to a
   native one, refusing to ship anything that isn't a Linux ELF binary.)
5. Connects with `tsh ssh -t <login>@<uuid> -- ~/beams/bin/beam-shell` -
   bypassing the `tsh beams ssh` layer (which can't take a remote command)
   in favor of plain `tsh ssh`, per project.md. `beam-shell` unlocks the
   ssh-agent key, kicks off `beam-bootstrap` in the background, and drops
   you into zellij with a log pane - see `../bootstrap/README.md`.

On reuse, steps 3-4 are skipped entirely - a reused beam is assumed to
already be bootstrapped from a prior run.

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
