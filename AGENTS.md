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

## Task runner

`bootstrap/src/tasks/` holds templated, on-demand tasks (distinct from
the fixed startup `steps/` — see `DESIGN.md`'s "Templates vs.
instances"). This is how to actually reach for one from an agent session,
today, not just a description of the design:

- `beam-bootstrap` itself is reachable by bare name below (no
  `./target/debug/` prefix needed) once a beam's been bootstrapped at
  least once — see `bootstrap/README.md`'s "PATH" section for the two
  steps (`link-tool-paths`/`shell-config`) that make that, and the Go/
  code-server toolchains, true.

- **Daemon-backed, the normal way to reach any template now**:
  `beam-bootstrap task list` / `task run <template> --param value ...`
  / `task run-manifest --file <path>` - talks to the already-running
  daemon over its socket (`~/.beam-bootstrap/daemon.sock` by default),
  submits, then blocks printing progress until it finishes, exiting
  0/1 accordingly. Covers every template - a unit test
  (`main.rs::tests::task_command_covers_every_known_template`) fails the
  build if a new template ever lands without a `task run` subcommand for
  it, same way `template_schemas_match_what_instantiate_actually_accepts`
  already guards `instantiate`. No more need to hand-roll JSON at the
  socket for this - see `bootstrap/README.md`'s "`task` - the
  daemon-backed CLI" section.
- **No daemon needed**: `beam-bootstrap list-tasks` /
  `run-task <template> --param value ...` runs one task in-process -
  same coverage as `task run` above (same `TaskCommand` enum drives
  both), just without the daemon's concurrent-wave scheduling or
  broadcast progress. Prefer `task run` above when a daemon's already up
  anyway (progress then shows in a connected TUI too), or for anything
  that needs its `exclusive_resources` tag actually serialized against
  other *concurrently running* tasks - a bare `run-task` only ever runs
  the one task you gave it, so that tag has nothing to serialize against
  there. Real cross-process contention (e.g. `apt-get`) goes through
  `aptlock`'s `flock(1)` regardless of which path you use, so that part's
  covered either way.
- **Standing permission**: run templated tasks (either path above)
  against a beam — including this session's own — without asking first.
  They're idempotent and scoped to exactly what they say (installing a
  toolchain, cloning a repo), same trust level as any other read/build/
  install command already run freely in this repo. This doesn't extend
  to the *startup sequence* or daemon lifecycle itself (don't restart
  `beam-bootstrapd`, don't re-run the fixed sequence against a beam
  that's mid-session) — only to invoking an existing template.
- **Need a toolchain on a beam (including this session's own) to build or
  test something?** Check for an existing install task first -
  `install-toolchain-go`/`install-toolchain-rust` (and anything named
  `install-*` added later) cover this. Reach for them (either path above)
  instead of hand-rolling `curl .../rustup.rs | sh`, `apt-get install gcc`,
  etc. yourself: idempotent (skips if already installed), tracked by a
  capability other tasks can depend on (`install-code-server` already
  requires the toolchains, same way a future task should), and leaves the
  beam in the same state a normal run would rather than a one-off manual
  install nothing else knows about. If `beam-bootstrap` itself isn't on
  this beam yet at all (no prior bootstrap run), that's the one case these
  can't help with directly - a Rust toolchain can't be installed *by* a
  binary that itself needs one to exist first; see this file's "No Rust
  toolchain on a stock beam" convention below for why that binary is
  always built elsewhere and copied over instead. Otherwise, even a bare
  `run-task install-toolchain-rust` (no daemon needed) beats installing
  it by hand.
- **Standing instruction**: if the same ad hoc multi-step shell sequence
  comes up more than once in a session (or across sessions, per
  `TODO.md`) — installing some tool, preparing some path, anything with
  real params and a check for "is this already done" — turn it into a
  new task template under `bootstrap/src/tasks/` instead of continuing
  to hand-roll it. That's the whole point of the task-graph design (see
  `DESIGN.md`'s "Why"): one reusable definition, reachable the same way
  by this session, a later session, the TUI, and eventually `beams task
  run` (see `DESIGN.md`'s architecture diagram — not implemented yet,
  tracked in `TODO.md`). Add it to `TODO.md` if you don't get to it
  immediately, so it isn't just forgotten chat context.

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
  available inside a beam itself. If a beam already has `beam-bootstrap`
  (any prior bootstrap run leaves it on `PATH` — see
  `bootstrap/README.md`'s "PATH" section) and you need `cargo`/`go` there
  anyway (e.g. to build and test a change to this repo's own Rust code,
  in-session), use its `install-toolchain-rust`/`install-toolchain-go`
  task instead of installing by hand — see "Task runner" above.
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
