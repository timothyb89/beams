# beams

`beams` bootstraps and manages ephemeral [Teleport](https://goteleport.com)
Beam VMs so they're actually usable for day-to-day dev work, despite having
no persistence and a hard 24h TTL.

It's two halves, each its own Rust crate:

- **`client/`** (binary `beams`) — runs on your *workstation*. Spawns or
  reuses a beam and drops you into it with `beams ssh`.
- **`bootstrap/`** (binary `beam-bootstrap`) — runs *inside* a beam. Links
  dotfiles, installs tools, sets up the shell, and joins the beam to your
  cluster.

Plus `protocol/` (shared types the two halves use to talk to each other),
`bot-onboarding/` (one-time cluster setup), `bin/` and `shell/` (dotfiles
symlinked into a beam), and `docs/` (design notes).

## Prerequisites

- A Teleport cluster with the Beams feature enabled, and an authenticated
  `tsh` session (`tsh login ...`).
- Rust (stable) to build the client and bootstrap binaries.
- On macOS, or to cross-compile for a beam from any host: see
  `bootstrap/build-cross.sh`.

## Quick start

```sh
# Build the workstation client
cd client
cargo build --release

# Build a Linux binary for the beam side
cd ../bootstrap
cargo build --release          # on Linux
# or: ./build-cross.sh         # cross-compiling from macOS/elsewhere

# Spawn or reuse a beam and drop into it
cd ../client
./target/release/beams ssh
```

`beams ssh` tracks a "default" beam per purpose, reuses it while it has
enough TTL left, copies this repo and the bootstrap binary onto it, and
runs the bootstrap steps automatically before dropping you into a shell.

See `client/README.md` and `bootstrap/README.md` for the full command
references (beam management, SSH key approvals, startup tasks, the task
daemon, etc).

## Docs

- `project.md` — original design notes/spec.
- `AGENTS.md` — working conventions for anyone (human or AI) hacking on
  this repo.
- `TODO.md` — live task list.
- `docs/designs/` — design docs and exploratory spikes for specific
  subsystems (the daemon/task-graph architecture, client-owned services,
  the SSH agent proxy, folder sync, OpenSSH multiplexing).
- [Configurable launches](docs/designs/LAUNCH_CONFIGURATION_DESIGN.md) —
  proposed client-side profiles, batched uploads/packages, and pre/post
  initialization tasks; not implemented yet.
- [Launch developer guide](docs/development/launches.md) — draft execution
  ordering and task extension guidance for the planned configuration system.

## Status

Personal project, actively changing. Expect sharp edges.
