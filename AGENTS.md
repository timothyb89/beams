# AGENTS.md

Guidance for AI agents (and future-you) working in this repo. This is a
personal project (`timothyb89` namespace) — no AI-authorship restrictions
apply here, unlike `gravitational/*` repos.

## What this repo is

`beams` bootstraps and manages ephemeral Teleport Beam VMs so they're
actually usable for day-to-day dev work despite having no persistence and
a 24h hard TTL. See `project.md` for the original design notes (still
mostly accurate — treat it as the spec, this file as working notes on top
of it, and `TODO.md` as the live task list).

Two halves:

- **`bootstrap/`** (Rust binary `beam-bootstrap`) — runs *inside* a beam.
  Idempotent, ordered steps (`bootstrap/src/steps/*.rs`) that link
  dotfiles, install packages/tools, set up fish/starship, join the beam to
  the cluster as a bot, and start `tbot` for k3s access. See
  `bootstrap/README.md` for the full step list and build instructions.
- **`client/`** (Rust binary `beams`) — runs on your *workstation*. Spawns
  or reuses a beam, stages+copies this repo onto it, and `tsh ssh`s in.
  See `client/README.md`.

Plus:
- `bin/` — scripts symlinked into a beam's `$HOME` (`beam-shell`,
  `ssh-agent-init`).
- `shell/` — fish config, starship config, zellij layout, all symlinked in.
- `bot-onboarding/` — cluster-level (not per-beam) Teleport role/bot setup
  for the `k3s-bot`, applied once via `tctl`.
- `.ssh/` — the shared ephemeral key both halves use (private key
  git-ignored; only pushed to a beam, never committed).

## Working conventions

- **Steps must stay idempotent.** Every `bootstrap` step checks what's
  already in place and reports `skip` instead of redoing work. Re-running
  the whole binary after a partial failure is the expected recovery path
  — don't add steps that assume a clean slate.
- **Steps run in a fixed order and stop on first failure** (see
  `bootstrap/src/steps/mod.rs`). If a step fails, nothing after it runs.
  Keep that in mind when reordering or inserting new steps — put anything
  a later step depends on strictly before it.
- **Event protocol**: steps report `start`/`ok`/`skip`/`error` via
  `bootstrap/src/events.rs`, rendered human-readable on a TTY or as
  JSON-lines otherwise. Keep new steps emitting through this, not raw
  `println!`, so `--json` and the human renderer both keep working.
- **Cross-compilation matters.** `client/src/stage.rs` prefers a
  statically-linked `x86_64-unknown-linux-musl` build of `beam-bootstrap`
  (via `bootstrap/build-cross.sh`) and refuses to ship anything that isn't
  actually a Linux ELF binary. If you change the bootstrap build setup,
  keep that fallback/validation logic honest.
- **No Rust toolchain on a stock beam.** `beam-bootstrap` is always built
  elsewhere and copied over (`tsh beams scp`) — never assume `cargo` is
  available inside a beam itself.
- **State that can't be derived should be tracked explicitly**: the client
  keeps `~/.config/beams-client/state.json` (purpose → beam id) because
  there's no server-side way to label a beam. Follow that pattern rather
  than re-deriving things from `tsh beams ls` output each time if it's
  mutable/local state.
- **Keep docs in sync with behavior.** Each of `bootstrap/README.md` and
  `client/README.md` documents its own binary's behavior in detail —
  when you change a step or a client flag, update the matching README in
  the same change. `project.md` is the original design brief; update it
  only if the design itself changes, not for incremental implementation
  details (those belong in the READMEs, `AGENTS.md`, or `TODO.md`).
- **This environment is itself an ephemeral beam.** Don't assume local
  state here (shell history, scratch files, uncommitted edits) survives —
  commit progress and task notes to the repo as you go, which is the
  point of this file and `TODO.md`.

## Session workflow (this environment is itself an ephemeral beam)

Every beam, including the one these sessions run in, is deleted ~24h
after creation with no persistence. The git repo (pushed to `origin`) is
the *only* thing that survives a session boundary — conversation history,
scratch files, and anything agreed-but-unwritten are gone the moment the
beam disappears, planned or not. That changes how work here should be
sequenced:

- **Start of session**: re-orient from the repo, not from memory of a
  prior conversation — read `AGENTS.md` (this file), `DESIGN.md`,
  `TODO.md`, and `git log` for recent commits before resuming work. Don't
  assume context from an earlier chat carried over.
- **Write decisions down before or alongside implementing them, not
  after.** If a design discussion in conversation reaches a conclusion,
  that conclusion isn't real until it's in `DESIGN.md`/`TODO.md` and
  committed — a beam disappearing mid-conversation loses anything that
  only exists as chat.
- **Commit early and often, with discrete commits.** Don't batch a
  session's work into one giant commit at the end — if the session ends
  (TTL, crash, interruption) mid-task, whatever's uncommitted is lost.
  Prefer several small, individually-sensible commits over one large one
  held until "done."
- **Update `TODO.md` in the same commit as the work it tracks** — check
  off/remove finished items, add newly discovered ones. A stale TODO is
  worse than no TODO, since the next session trusts it at face value.
- **Push, don't just commit locally, once work is in a reviewable state**
  — a local commit on a beam that gets recycled before it's pushed is
  just as lost as an uncommitted change. (Still only push when actually
  asked to, per the attribution/workflow conventions already in place —
  the point is not to let pushing lag so far behind committing that a
  beam's disappearance costs real work.)

## Testing

- `client/src/stage.rs` has unit tests today (staging-directory assembly,
  binary validation). `bootstrap`'s steps have none yet — most touch real
  system state (apt, symlinks, systemd-less background services via
  `beamctl`) and would need either mocking or an actual beam to exercise;
  see `TODO.md`.
- There's no CI configured in this repo yet (no `.github/workflows`) —
  testing today is manual: build, run against a real or disposable beam,
  check `--json` output and re-run idempotency.

## Building

See `bootstrap/README.md` and `client/README.md` for full instructions.
Quick reference:

```sh
# bootstrap binary, cross-compiled static build for shipping to a beam
./bootstrap/build-cross.sh

# client wrapper, run from your workstation
cd client && cargo build --release && ./target/release/beams ssh
```
