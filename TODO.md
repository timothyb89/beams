# TODO

Live task list. See `docs/designs/DESIGN.md` for the current architecture direction
(daemon + capability-based task graph) and `project.md` for the original
brief. Check off / remove items as they land, add new ones as they come
up. Per `AGENTS.md`'s session workflow: update this in the *same commit*
as the work it tracks — this file, not conversation history, is what the
next session trusts.

Once a whole section above is done, move it out of this file into
`archive/<something-descriptive>.md` (one file per completed
project/milestone, roughly) rather than leaving it to accumulate here
indefinitely — this file should stay focused on what's still open. "Done
so far" context blurbs for small completed items can just be trimmed or
dropped; archiving is for a section's worth of finished work, not every
individual checked-off line.

## Configurable environment launches

Proposed 2026-11-06 in `docs/designs/LAUNCH_CONFIGURATION_DESIGN.md`: client-owned TOML
profiles compile into a versioned launch graph, with small pre-interface
and background post-interface bundles/package batches. Reuse the existing
executor and client transport. The design records current launch behavior,
inheritance rules, hook semantics, recovery, migration, and acceptance
criteria. The overall plan and graph/readiness-condition direction are endorsed;
offline schema v1 is implemented and documented in the developer guide.
The archive recommendation is
standard tar with independent none/gzip compression, initially testing
gzip level 1; complete verification/extraction releases artifact readiness.
Script paths select local files relative to their declaring profile;
entrypoints are automatically selected and packed (SSH transport pending).
An optional `script_root` selects
a helper directory to preserve on the Beam, with phase-aware packaging.

The [2026-11-06 handoff](docs/designs/LAUNCH_CONFIGURATION_DESIGN.md#implementation-status-and-next-session-handoff)
records completed commits, execution limits and validation. Daemon
acceptance, waiting conditions and snapshots, the shell/TUI readiness gate,
TUI snapshots, bridge uploads/submission with receive-driven release and
`beams config launch` are done (2026-11-06). Next: the opt-in `beams ssh`
path (runtime delivery, shell preparation, Zellij/TUI with
`BEAMS_LAUNCH_ID`), before changing live SSH startup.

- [x] Immediate prerequisite from review of `06060b5`/`651fc1c`: remove
  the stale forced repo `.ssh` link in `bootstrap/src/steps/link.rs`.
  The current tracked tree no longer contains `.ssh`, but the synchronous
  `link-files` pre-step still requires it and fails before Zellij on fresh
  Beams. Preserve Beam-managed `~/.ssh`, update link-step docs/comments,
  and validate using a clean-checkout fixture without local leftovers.
  Completed: only `bin` is linked. Isolated filesystem fixtures cover a
  clean checkout, repeat/skip behavior, and preservation of both real and
  symlinked Beam SSH directories, including old repo `.ssh` leftovers.
- [ ] Pre-seed `~/.ssh/known_hosts` and git config on new Beams. Found
  2026-11-06: this Beam had no `~/.ssh` (only a dangling pre-`38109e3`
  link to the repo's), so `git push` failed host key verification. Git
  also had no `user.name`/`user.email`. Since the repo's `.ssh`
  (which carried known_hosts) was dropped in `06060b5`/`651fc1c`, nothing
  seeds them. Confirmed on a fresh Beam 2026-11-07: every startup
  manifest `git@github.com:` clone fails "Host key verification failed"
  (no TTY to ask) before the agent relay is ever asked to sign.
  - [x] known_hosts: live lookup instead of seeding, landed 2026-11-07.
    `/etc/ssh/ssh_config.d/beams-known-hosts.conf` sets
    `KnownHostsCommand beam-bootstrap client known-hosts %H`; the daemon
    routes a `KnownHostsQuery` to the workstation (agent routing rules),
    which answers with `ssh-keygen -F` over the files `ssh -G` names.
    Verified on a disposable Beam: github.com reaches authentication,
    an unknown host still fails, `~/.ssh` untouched. Remaining: pinned
    GitHub keys in the core profile for headless launches without a
    bridge; ssh queries twice per connection (harmless).
  - git config: `user.name`/`user.email` defaulted from the workstation's
    `git config` at compile time, overridable per profile, applied with
    `git config --global` (idempotent; leaves other Beam settings alone).
    Optional: SSH commit signing through the relay (`gpg.format = ssh`,
    `user.signingkey = "key::<allowed key>"`); each signature then prompts
    like any SSHSIG request.
- [ ] Phase 1: record cold/warm launch milestones and transport counts;
  implement profile resolution/inheritance, validation, versioned plan,
  and `beams config init/validate/explain`. Bundled core/development
  profiles plus gitignored local configuration; keep new launch opt-in.
  Remaining here: timing/transport baseline, `config init`, workstation
  profile selection; snapshot/digest/launch identity tooling is now present.
- [x] First compiler slice: typed strict schema/plan, inherited source
  provenance, offline `config validate/explain`, embedded core/development
  starters, gitignored local off-ramp, and team override/helper fixtures.
  Package union/promotion, script/file artifact assignment, custom template
  parameters, automatic built-in prerequisites, dependency/phase/cycle and
  destination validation are covered. The compiler does not execute or
  upload; live `ssh` remains unchanged.
- [x] Compiler developer documentation: supported schema, command examples,
  annotated team fixture/explain output and checked script/file/package
  ordering in `docs/development/launches.md`; binary READMEs updated.
- [x] Profile payload snapshot/receive foundation: shared typed launch and
  bundle declarations, UUID/plan digest, phase-local deterministic tar and
  gzip-level-1 spools, deferred `config pack-post`, frozen replay, ordinary
  modes/empty directories/internal symlinks, hashed manifest entries and
  verified atomic read-only staging via `beam-bootstrap launch unpack`.
  Tests cover codecs, deferral, source edits, metadata, bad hashes/bindings,
  truncation, modified staging, unsupported files and malformed entries.
  Runtime/Zellij assets and SSH upload/readiness wiring remain pending.
- [x] Foreground execution foundation: `launch run-phase` adapts generic
  packages/files/scripts and native templates to the existing executor;
  post explicitly requires headless mode and a matching successful pre
  receipt. Verified staging, launch-scoped dependency identities and phase
  locks; whole-graph/native-template validation before effects; named JSON
  task/progress events.
  Directory merges, overwrite/backup policies, ordinary modes/internal links,
  protected home-directory merge boundaries,
  hook helpers/cwd/env, checks, content-based `on-change` records and explicit
  interrupted-hook retry are covered. Failure blocks dependents, unrelated
  work continues, and failed pre retries revoke old success receipts.
- [x] Shared package helper probes architectures/versions, batches installs
  under aptlock's cross-process lock, fingerprints sources/keyrings for
  refresh reuse and emits bounded subprocess progress. Rust releases apt
  before rustup and keeps toolchain destination locks. Fake-apt fixtures
  cover batches, refresh changes, failures and concurrency. CLI smoke with
  an isolated HOME proves pre/post gates, deferred bundles, helper/file
  execution, repeat skips and apt/independent-hook overlap without real apt.
- [x] Verification exposed an existing daemon acceptance-order race:
  local and bridge manifests now synchronously queue their acceptance
  before starting the worker, so start/progress/completion cannot overtake
  it. A delayed-acceptance empty-manifest regression fixture covers this.
- [x] Daemon launches: `launch_submit/signal/snapshot/retry` ops and
  `beam-bootstrap launch submit/signal/status/retry`. Durable acceptance by
  UUID/digest (identical resubmission = same launch, conflicting digest
  refused before effects), one worker per launch on `Hub.resources`,
  waiting `artifact-ready`/`interface-ready` conditions (daemon-verified
  staging; interactive or headless gate), per-node state with bounded logs
  persisted on transitions, a daemon-wide `seq` making state-carrying
  replies exact event-stream boundaries, restart -> interrupted, explicit
  retry. Hooks get the relay `SSH_AUTH_SOCK`. Fixture tests plus
  `bootstrap/tests/launch-daemon-smoke.sh`.
- [x] Readiness and bridge launches (2026-11-06): `launch_ready` (shell via
  `shell/fish/conf.d/beams-launch.fish` at the first prompt, the TUI after
  its first frame with the launch's snapshot) releases interactive
  `interface-ready`; the Beam TUI loads `launch_snapshot` on connect and on
  acceptance; `launch` bridge capability with chunked resumable uploads
  (32 KiB, window 8), submission by ID/digest, owner-routed
  `LaunchStatus`/`LaunchCondition`; the Beam stages each complete bundle
  itself (also `launch receive`); workstation `beams config launch`
  re-drives unfinished launches on every bridge connection. The bridge
  writer no longer polls (it capped all bridge messages at 20/s).
  `client/tests/launch-bridge-smoke.sh` runs it end to end.
- [ ] Launch daemon follow-ups: reconcile owned subprocesses after a
  daemon restart before allowing retry to overlap an old installer; a
  cancel op (a condition otherwise waits until signalled); retry over the
  bridge (an interrupted launch is only retried on the Beam today);
  cleanup of finished launches' workstation launch directories and Beam
  staging; `receive` re-copies an already verified upload into its spool
  (one extra copy of a large post bundle).
- [ ] Measure upload throughput over a real `tsh` bridge (window/RTT bound,
  base64 overhead) against `tsh scp` before tuning the window or adding a
  second transport for very large post bundles.
- [ ] Flaky: `forward::tests::startup_failure_includes_captured_diagnostics`
  failed once (exit 23 reported without the captured stderr) and passed on
  rerun; likely a race between process exit and draining its stderr.
- [ ] Developer documentation is required alongside implementation:
  evolve `docs/development/launches.md` with the compiler/schema, an
  execution-order graph, explicit scripts-vs-packages/file dependencies,
  ready conditions, resource locks, task extension instructions, and
  retry/reconnect diagnosis. Deliver an annotated profile with matching
  explain output and fixture coverage; update binary READMEs in the same
  changes. Compiler behavior/examples and foreground execution/retry are
  documented, and so is the daemon path (ops, conditions, snapshot
  boundary, retry/restart), readiness reporting and the bridge upload flow;
  a walkthrough of a real `beams ssh` launch remains with that phase.
- [ ] Phase 2 remaining: parameterize specialized installs (versions,
  destination paths and profile environment). Source/keyring preparation
  dependencies, privileged file destinations and configurable admission/
  concurrency bounds remain. Foreground adapters/package transactions,
  Rust's apt split, and daemon acceptance/shared resources/snapshots are
  implemented; this is not live startup yet.
- [ ] Cross-binary smoke, remaining part: `launch-daemon-smoke.sh` covers
  deferred bundles, explicit gates, resubmission, retry refusal and daemon
  restart, and fixture tests cover resource serialization, but no committed
  cross-binary test yet runs fake apt with an independent hook completing
  while apt waits.
- [x] Phase 3: ship a small runtime/pre archive independently of a Git
  checkout; run pre tasks through the daemon, prepare the shell before
  spawning it, and gate post tasks on shell startup + first TUI frame.
  Remove the new path's competing background fixed bootstrap. Landed
  2026-11-07 as opt-in `beams ssh --profile` / `[launch] profile`
  (`~/.beams/` runtime, `launch start`, per-launch Zellij session,
  `launch shell` readiness wrapper); validated on disposable Beams.
- [x] SSH grants for launch tasks (2026-11-07): profile `[ssh_grant]`
  (hosts/user/window/max_uses/tasks), compiled to workstation-only
  `ssh-grant.json`; limited to the covered tasks' exact `BEAMS_TASK_RUN`
  tags; window opens at acceptance (pre) or `interface-ready` (post);
  ended on finish/disconnect; issued once per launch. The Beam routes a
  running launch's tasks to its owning workstation. Remaining: a real-key
  clone on a disposable Beam (needs an approved key; fixture + driver
  tests only so far).
- [ ] Remove the legacy startup once the profile path is the default
  (maintainer decision 2026-11-07: no long-term legacy support, single
  user): the `[startup]` manifest submission (`clientd::submit_startup`,
  `startup.rs`, `RunManifest` startup grants unless launch tasks reuse
  them), `bin/beam-shell`'s fixed background run and the Git-push
  delivery, the `~/beams/bin/beam-client-bridge` fallback, and the
  implicit Go/Rust defaults, and the fixed `install-starship` step (now
  the `install-starship` template in `builtin:development`'s pre). The
  maintainer's clones and grant are in gitignored
  `environments/local/personal.toml` (2026-11-07; `KnownHostsCommand`
  and launch grants landed). Still to decide: which other fixed steps
  (claude update/CLAUDE.md merge, kubectl, teleport binary from the
  staging CDN, k3s bot/tbot/kubeconfig) become profile tasks.
- [x] `beams ssh` never reused a Beam once the maximum Beam TTL dropped to
  12h (`MIN_TTL_HOURS` was 12; ~11h left observed 2026-11-07). Now 6h,
  plus `beams ssh --beam ID|UUID|PURPOSE` to connect to a specific Beam
  regardless of time left.
- [ ] Profile-launch follow-ups: a reattached Beam keeps the runtime it
  was launched with (no runtime upgrade path); `launch start`'s pre
  progress repeats `pending` lines; measure scp vs. bridge upload for
  the runtime; `--no-client-daemon` is refused rather than running
  local-only work.
- [ ] Phase 4: projected upload progress in the session overlay/Beam TUI,
  bounded task concurrency and retry/recovery. Background uploads and
  receive-driven release of artifact-ready landed 2026-11-06 (`beams config
  launch`); submitting-client routing for SSH key use landed with the SSH
  agent follow-ups.
- [ ] Phase 5: migrate the maintainer's setup onto a local profile,
  distribute a sanitized development default, adapt legacy `[startup]`,
  document customization, and validate on disposable Beams before
  switching defaults.
- [ ] Phase 6: optional daemon-owned OpenSSH ControlMaster for preparation
  and uploads after the workstation compatibility matrix passes. Share
  the Phase 7 client-services transport work below, not a second backend.
- [ ] Acceptance: no optional post work gates the first interface;
  normal setup uses at most two apt install transactions and two archive
  uploads; apt/downloads overlap; reconnect restores task state; packaged
  launches need no source checkout or maintainer-specific configuration.

## Done so far (for context, not actionable)

- `beam-bootstrap`: dotfile linking, apt packages, fish/starship,
  kubectl/zellij install, k3s-bot join + `tbot` as a background service,
  kubeconfig symlink, `claude update`. Idempotent, JSON or human output.
  Current implementation: a fixed, run-once step list — see "Migration"
  below for how this becomes the daemon/task-graph's zero-param
  templates.
- `beams` client: spawn-or-reuse by purpose, concurrency cap, git push of
  the repo + scp of a prebuilt bootstrap binary (no SSH key), `tsh ssh`
  into `beam-shell`.
- `bin/beam-shell`: zellij layout with the main pane next to the bootstrap
  TUI and log tail. SSH goes through the daemon's agent relay (see "SSH
  agent proxy" below); there is no unlock pane anymore.
- `AGENTS.md`, `docs/designs/DESIGN.md`: architecture/conventions + the
  daemon/task-graph design written up and agreed.

## Migration to the daemon/task-graph design (see docs/designs/DESIGN.md)

Supersedes the old "bootstrap wizard" / "bidirectional progress
reporting" items from the previous version of this file — both are now
just consequences of this design rather than separate asks.

- [x] Capability-based dependency resolution: `bootstrap/src/graph.rs`
  (`Node` trait, `resolve_order`), wired into `steps::resolved()`. Every
  existing step now declares `provides`/`requires` (see `steps::cap`)
  instead of relying purely on hand-ordering. Confirmed no behavior
  change via `steps::tests::resolved_order_matches_original_hand_ordered_list`.
- [x] Template/instance types: `bootstrap/src/tasks/` — `Task` trait
  (runtime instance: owned id + `Capability`s, mirrors `Step` but
  params-derived) and `Template` trait (marker: `NAME` + instantiable
  into a `Task`). `graph::Capability` is now `Cow<'static, str>` (was
  `&'static str`) so a step's compile-time constant and a task's
  runtime-computed string can share one graph — see
  `graph::tests::owned_and_borrowed_capabilities_are_interchangeable`.
  No param-schema introspection yet (deliberately — see `Template`'s doc
  comment: nothing needs to enumerate/introspect templates until the
  daemon/manifest work below actually exists to consume it).
- [x] `clone-git-repo(repo, dest)` and `checkout-pr-worktree(base, pr,
  dest)` — implemented as the first two templates
  (`tasks::clone_git_repo`, `tasks::checkout_pr_worktree`), each with
  tests exercising real `git` operations against local-only repos (no
  network needed). `checkout-pr-worktree` only supports the initial
  checkout, not updating an existing worktree to a PR's later pushes —
  tracked as a known gap in its doc comment, not fixed.
  `checkout-pr-worktree`'s `requires` on `clone-git-repo`'s capability is
  **not yet checked by anything** (see next item) — only by its own
  `run()` probing `base`'s filesystem state directly.
- [ ] Reachable today only via `beam-bootstrap list-tasks` /
  `run-task <template> ...` — a direct, in-process stand-in for the
  daemon-backed CLI, added now so the template mechanism has a real
  caller. Runs exactly one task with no dependency resolution against
  anything else; superseded once the daemon exists.
- [x] **Former known gap, now resolved**: `Step` and `Task` couldn't be
  resolved/run together in one graph. Didn't need merging into one
  trait — `steps::StepAsTask` adapts a `Step` into a `Task` (wraps the
  repo path it needs at construction), so the fixed startup sequence and
  templated instances now share one graph/executor. See "Concurrent
  execution" below for why this got settled now rather than later (it's
  what made `apt-install`-concurrent-with-`clone-git-repo` possible at
  all).
- [x] **Concurrent execution** (see docs/designs/DESIGN.md's "Concurrent execution"):
  `graph::resolve_levels` groups tasks into dependency waves instead of
  one flat order; `executor::run` runs each wave's tasks on their own
  threads, serializing only pairs sharing a `Task::exclusive_resources`
  tag (added to `Step` too, via the adapter above), and stops advancing
  to the next wave on a failure without trying to cancel in-flight work.
  `steps::run` (the fixed startup sequence) and `daemon::run_manifest`
  both go through this now, instead of each having their own serial
  loop. `apt-install`/`install-kubectl` tagged with a shared `"apt"`
  resource — the concrete case that would've broken immediately
  otherwise (both shell out to `apt-get`/`dpkg`, no capability
  dependency between them to have caught it). Verified: unit tests
  (independent tasks measurably run concurrently not serially;
  exclusive-resource tasks provably never overlap; a mid-wave failure
  doesn't stop the rest of its own wave but does stop later waves) *and*
  two full real runs of `beam-bootstrap --repo ~/beams` against this
  beam's actual state — one surfaced a real pre-existing bug (below),
  the other completed cleanly with visible concurrent waves in the
  `--json` output and `install-kubectl` visibly waiting on the `apt`
  lock.
  - **Found along the way, not fixed (separate from this work)**: this
    beam's `~/.config/fish/config.fish` was a real file (fish's own
    auto-generated default, written before `shell-config` ever claimed
    that path), not a symlink — `shell-config` doesn't force-replace the
    way `link-files` does for `.ssh`. Probably related to, or the same
    root cause as, the "starship + fish" item already below. Worked
    around locally (moved the file aside) to finish validating; the
    real fix still belongs with that item.
  - [x] Wave/concurrency structure now on the wire: `run_manifest`'s
    `accepted` reply carries `waves` (list of task-id lists, one per
    wave) instead of a flat `task_ids`; every `task_status` broadcast
    carries a `wave` index (`null` for a standalone `run_task`). Lets a
    client group "running right now" rows by `wave` instead of inferring
    concurrency from event timing.
  - [x] Param schema for templates: `tasks::TemplateInfo`/`ParamSchema`
    (name/kind/required per param), hand-written next to each
    template's own params struct in `instantiate` (no reflection crate
    for two templates' worth of fields) and cross-checked by a test that
    builds each template's params from *only* its declared schema
    fields and confirms `instantiate` accepts them. `list_tasks`'
    reply and `beam-bootstrap list-tasks` both show it now.
- [x] Daemon skeleton: `beam-bootstrap daemon` (`src/daemon.rs`) — UDS
  socket (`--socket`, default `~/.beam-bootstrap/daemon.sock`),
  newline-delimited JSON, `request_id`-correlated direct replies,
  `task_status` events broadcast to every connected client (not just the
  requester — verified both in-process (`UnixStream::pair()` tests) and
  against a real socket file with two concurrent client connections).
  Supports `list_tasks`/`run_task` only, going through the same
  `tasks::instantiate` the CLI's `run-task` uses. Refuses to start a
  second instance against an already-listening socket. Thread-per-
  connection + a global run-lock (std threads/mpsc, no async runtime —
  stayed with the "boring" approach docs/designs/DESIGN.md calls for; a handful of
  local connections doesn't need one). Not yet wired to anything that
  starts it automatically (`beamctl start ...`, see next item) or to the
  fixed startup sequence.
- [ ] `beam-bootstrap --repo ...` (today's one-shot entrypoint) becomes:
  ensure a daemon is running, then let the daemon run the startup tasks,
  rather than running them itself inline.
- [x] Manifest format + `run_manifest` socket op (`daemon.rs`): a list of
  `{template, params}` entries, resolved against *each other* (not
  against any prior run's state - see the module doc comment's note on
  why, and the idempotent-redundant-entry workaround) via
  `graph::resolve_order` over their instantiated `Task`s, then run in
  that order, stopping at the first failure (same semantics as
  `steps::run`). Verified: out-of-order entries get reordered correctly,
  an unsatisfiable dependency is rejected before anything runs, and a
  mid-manifest failure stops the rest - both as daemon unit tests and
  manually against a real socket file with `checkout-pr-worktree` +
  `clone-git-repo` requested together, in reverse order. Each task's
  `task_status` broadcast carries a `manifest_id` when run this way
  (`null` for a standalone `run_task`); the whole manifest holds
  `Hub::run_lock` for its full duration, not just per task, so it runs as
  one atomic unit relative to other connections' requests.
- [x] **Fixed, landed together with the `beams task run` item below** -
  turned out to be the same piece of work. `beam-bootstrap task
  {list,run,run-manifest}` (`main.rs`, new `taskcli.rs` for the
  synchronous one-shot socket client) - daemon-backed, submits then
  blocks printing every `task_status`/`manifest_status` broadcast for
  what it just submitted until a terminal state, exiting 0/1
  accordingly. `run-manifest --file <path>` reads the same
  `[{"template","params"}, ...]` JSON shape the README's `nc -U`
  example hand-writes. Verified live against a real throwaway daemon:
  `task list`, a `clone-git-repo` run (then a re-run confirming the
  skip path), a two-task manifest (`clone-git-repo` +
  `checkout-pr-worktree`), and both single-task and manifest error
  paths (bad params, an unresolvable PR ref) all produced the right
  output and exit code. Covered by two new `taskcli` tests against a
  real `daemon::serve` listener.
- [x] `beam-bootstrap tui` (`src/tui/`): ratatui client, connects to an
  already-running daemon (`tui::client::DaemonClient`). Top half:
  template list + param-entry form built from `list_tasks`' schema
  (`tui::app`). Bottom half: one row per task mentioned by *any*
  connection's broadcasts (not just this TUI's own), tagged with its
  wave so concurrently-running tasks group visibly, plus an aggregate
  gauge. Verified both with unit tests on `App`'s pure state-transition/
  JSON-building logic (no terminal needed) and live, end to end, against
  a real daemon in a scripted tmux session: submitted a task through the
  TUI and watched it complete, then submitted a 2-task concurrent
  `run_manifest` from a *separate* connection and confirmed the TUI (as
  a bystander) rendered both rows tagged `[w0]` with the gauge updating
  correctly.
  - **Known simplification, not a bug**: only submits one task at a
    time (`run_task`), even though the daemon can resolve/run several
    together via `run_manifest`. No multi-select-and-submit-as-manifest
    UI yet — picking several templates, filling in params for each, and
    sending them as one manifest. Worth adding once there's a concrete
    second use case beyond "clone then checkout a PR" to shape the UX
    against (that pair can already be done as two sequential `run_task`
    submissions from the TUI today, just not concurrently with each
    other since they're not in a shared capability chain anyway).
  - [x] **Fixed**: the gap found restarting the daemon mid-E2E-test (TUI
    went quiet forever on a dropped connection) - `DaemonClient::reconnect`
    (`tui/client.rs`) opens a fresh socket connection + reader thread in
    place; `run_loop` (`tui/mod.rs`) detects the drop via the event
    channel's `TryRecvError::Disconnected`, shows it in the status line
    (`App::mark_disconnected`), and retries `reconnect` once a second
    (`RECONNECT_RETRY_INTERVAL`) until it succeeds, re-fetching the
    template list on success (`App::mark_reconnected`). Rows from before
    the drop are left as-is - reconciling what happened to in-flight
    tasks during the gap isn't attempted. Covered by a real-socket test
    (`tui::client::tests::reconnect_recovers_after_the_daemon_restarts`)
    simulating an actual daemon-process restart (same UDS path, old
    listener/connection dropped, new one bound) plus two `App` unit
    tests for the status-line/re-fetch behavior.
- [x] **Fixed, see the `run_manifest` CLI sugar item above** - `beam-bootstrap
  task run <template> --param value ...` is this, landed as the same
  change.
- [ ] Wire manifest seeding into `client`'s `beams ssh`/`beams new` (e.g.
  `beams new --clone <pr>` builds a manifest and hands it to a freshly
  spawned beam before its first startup pass). **Partly done:** the
  per-user startup manifest is now client-owned (`[startup]` in the
  client's `config.toml`, sent over the bridge as `RunManifest` once per
  Beam; see "SSH agent proxy" below). Still open: per-invocation
  additions (`--clone`, `--pr`) merged into that first manifest, ideally
  covered by the same startup grant.

## New tasks enabled by the graph (deferred-by-default templates)

`clone-git-repo`/`checkout-pr-worktree` landed — see the "Migration"
section above for status/details.

- [x] `install-toolchain-go` (`tasks/install_toolchain_go.rs`) —
  official Go release, pinned version + checksum (go.dev doesn't
  publish per-file `.sha256` sidecars like GitHub releases do - fetched
  the real checksums from its JSON release manifest directly rather
  than guessing). Installs to `~/.local/toolchains/go`, no `PATH`/shell
  changes attempted.
- [x] `install-toolchain-rust` (`tasks/install_toolchain_rust.rs`) - via
  `rustup` (same command `bootstrap/README.md` already told a human to
  run), installs `gcc` via apt first (cargo needs a linker). Tagged with
  `steps::resource::APT` since it shells out to `apt-get` - the first
  *task* (as opposed to fixed *step*) to need that tag, confirming the
  mechanism actually works across the two.
- [x] **Fixed**: the cross-process apt/dpkg race (`Task::exclusive_resources`
  only serialized tasks within *one* `executor::run` call, not across
  the fixed startup sequence's process and a later daemon-driven
  manifest's). New `aptlock.rs`: every `apt-get` call site
  (`steps::apt`, `steps::kubectl`, `tasks::install_toolchain_rust`) now
  goes through `aptlock::apt_get`, which wraps the real invocation in
  `flock(1)` against a well-known lock file
  (`~/.beam-bootstrap/apt.lock`) - no new dependency, and the lock
  releases itself the instant the wrapped process exits, crash or not.
  (`flock`'s own arg parsing has no `--` options terminator, unlike most
  GNU tools - learned the hard way, see the function's comment.) The
  in-process `exclusive_resources` tags stay too, since they avoid even
  *starting* a second apt-touching task concurrently within one process
  rather than starting it and blocking inside the lock. Verified: a unit
  test that 5 real concurrent `flock`-wrapped processes correctly
  serialize a counter increment (would under-count on a lost update if
  unserialized), and live against this beam's real `apt-get` - three
  genuinely concurrent `--only apt-install` invocations, all succeeding
  cleanly through the lock.
- [x] `install-code-server` (`tasks/install_code_server.rs`) - browser-based
  editor hosting, closing the "Explicitly deferred" item that used to live
  further down this file. Alternatives considered: `openvscode-server`
  (same tarball-based install shape, smaller community, no concrete
  reason to prefer it over code-server today); Eclipse Theia (no
  standalone-binary release, expects Docker or a Node build - doesn't fit
  this repo's "download a pinned static binary" pattern); `ttyd`/`gotty`
  + neovim/helix (lightest weight, but this beam already has rich
  terminal access over `tsh ssh` + zellij - the point of this task is a
  *browser* UI). Went with **code-server**: self-contained release
  tarball (bundles its own Node runtime), defaults to the Open VSX
  extension gallery (not Microsoft's, for licensing), which has both
  `golang.go` and `rust-lang.rust-analyzer`. No published checksum
  sidecar for its GitHub releases (same situation `install-toolchain-go`
  already handles for go.dev) - pinned `v4.141.0`'s amd64/arm64 tarball
  hashes from a verified download instead. Requires
  `install-toolchain-go`/`install-toolchain-rust` to have already run
  (gopls/rust-analyzer need an actual toolchain to drive); declared via
  `requires` for the daemon's manifest graph, and also self-checked in
  `run()` directly for a bare `task run`, same pattern
  `checkout-pr-worktree` already uses for its own prerequisite. Backgrounds
  it as a `code-server` beamctl service bound to `127.0.0.1:8080 --auth
  none` - loopback + port 8080 matches what `tsh beams publish` (HTTP
  mode) actually forwards per `~/AGENTS.md`; `--auth none` is fine since
  Teleport's app proxy is the real auth boundary, same reasoning as the
  daemon's own unauthenticated UDS socket. Publishing itself stays a
  separate, manual `tsh beams publish $BEAM_ALIAS` step - this task only
  installs and starts it.
- [x] **Gap found while verifying the task above, now fixed**: `task
  run`/`run-task` (`main.rs`'s `TaskCommand` enum) only had subcommand
  variants for `clone-git-repo`/`checkout-pr-worktree` - every zero-param
  template (`install-toolchain-go`, `-rust`, and now `-code-server`) was
  reachable only via `run-manifest`, not `task run` directly. Added the
  three missing variants (each mapping to `Template::NAME` + `json!({})`
  in `task_command_to_instance`, the shared helper both `run-task` and
  `task run` already went through) and a unit test,
  `task_command_covers_every_known_template`, that builds `TaskCommand`'s
  actual clap subcommand list via `Subcommand::augment_subcommands` and
  diffs it against `tasks::names()` - fails the build the next time a
  template lands without a `task run` variant, same drift-guard role
  `template_schemas_match_what_instantiate_actually_accepts` already
  plays for `instantiate`.

## PATH management

- [x] **Fixed**: `beam-bootstrap` itself, and the Go/code-server
  toolchains' `bin/` dirs, weren't on PATH anywhere outside the
  `BEAM_BOOTSTRAP_BIN`-exporting dance `bin/beam-shell`'s own zellij
  session already did internally - a plain `tsh ssh` or a fresh shell
  outside that one session had none of it. New `link-tool-paths` step
  (`steps/tool_paths.rs`) symlinks whichever binary is running it to
  `~/.local/bin/beam-bootstrap` (sidesteps re-deriving `bin/beam-shell`'s
  own musl-release/native-release/native-debug fallback chain - whatever
  binary is running *this step* already answers that question), and
  creates (empty, if not already installed) `~/.local/toolchains/go/bin`
  and `.../code-server/bin`. New `shell/fish/conf.d/beams-path.fish`
  (linked by the existing `shell-config` step) adds all of those plus
  `~/bin` to fish's PATH via `fish_add_path` - `~/bin` needed this too,
  since it's only on bash's PATH via `~/.profile`'s Debian-default
  block, which fish (the beam's actual default shell) never sources.
  `~/.cargo/bin` isn't part of either piece - rustup's own installer
  already wires that into both bash and fish on its own.

  The empty-directories-created-ahead-of-time part is what makes the
  "already open shell picks up a tool installed later" trick actually
  work: `fish_add_path` silently skips a path that doesn't exist yet, so
  without pre-creating these, the first-ever shell startup (before any
  toolchain is installed) would never add them, and no later shell would
  either since `fish_add_path` dedupes - the entry would just never
  arrive. With the directory already present, `fish_add_path` stores it
  in fish's *universal* `$fish_user_paths` variable, which fish syncs
  live across every other already-running fish instance - confirmed
  directly with a throwaway pty-backed interactive fish session (needed
  `TIOCSCTTY`, not just a plain pty, for fish's job-control setup to work
  at all - a non-interactive/no-controlling-tty fish never reaches the
  code that notices a universal variable changed): a second process's
  `fish_add_path` call for a brand-new directory showed up in `which`/
  `$fish_user_paths` in the already-running session within about a
  second, no new shell or `source` needed.

## zellij layout fix (independent — doesn't need the daemon work)

- [x] Fixed `shell/zellij/beam-shell.kdl`: confirmed the root cause by
  dumping a running session's actual layout (zellij writes one to
  `~/.cache/zellij/contract_version_1/session_info/<name>/session-layout.kdl`)
  — the old flat `layout { pane ... }` (no explicit `tab` block) was
  literally being used as the session's `new_tab_template`, so every new
  tab replayed the full log-tail/ssh-agent structure. Fix: explicit
  `tab name="beam-shell" { ... }` for the first tab, separate minimal
  `new_tab_template` (zellij's stock single-pane shape) for every tab
  opened afterward. Verified live: started a throwaway `zellij -s
  test-layout -n shell/zellij/beam-shell.kdl` session, opened two more
  tabs via `zellij -s test-layout action new-tab`, and `action
  dump-layout` showed the first tab keeping its full 3-pane structure
  while both new tabs got the plain single-pane template.
- [x] `beam-bootstrap tui` is now wired into `bin/beam-shell`'s session:
  baked directly into the first tab's explicit layout (as a right-hand
  pane, `size="30%"`) rather than via `zellij action new-pane` - safe to
  do now that the first tab has its own explicit block, separate from
  `new_tab_template`, so it only ever applies once, not per-tab. A new
  fixed step, `start-bootstrap-daemon` (mirrors `start-tbot`'s
  beamctl-service pattern), runs synchronously in `bin/beam-shell`
  before zellij launches, same as `link-files`/`install-zellij`/etc., so
  the daemon the tui pane connects to is already up by the time zellij
  starts (the tui's own connect also retries for 5s regardless, as
  insurance, not as the primary mechanism). Verified live: ran the new
  step for real on this beam (now a genuine `beamctl` service,
  `beam-bootstrapd`), then rendered the actual updated layout file in a
  throwaway zellij session (not the real `beam-shell`/`SESSION="beam"`
  path, to avoid touching this environment's own live session) and
  confirmed the tui panel renders correctly alongside the existing
  log-tail structure.
- [x] Right-hand side is now a `stacked=true` pane (tui `expanded=true`,
  log tail collapsed to its one-line title underneath) instead of a
  separate left-side split - stock zellij stacked-pane feature, matches
  "log viewer stacked with the TUI, collapsed by default" exactly.
- [x] Main shell pane no longer double-prompts for the ssh key
  passphrase alongside the dedicated `ssh-agent` pane: root cause was
  `~/.config/fish/config.fish` (every fish shell, not just the dedicated
  pane) sourcing `ssh-agent-init.fish`'s interactive unlock. Fixed via
  `BEAM_SHELL_SSH_UNLOCK_DELEGATED=1` (exported by `bin/beam-shell`
  before `exec zellij`, so every fish pane inherits it and skips its own
  unlock) which the dedicated `ssh-agent` pane explicitly `unset`s first
  so it's still the one place the prompt shows up.
- [x] `beam-bootstrap tui` mouse support: scroll wheel now moves the
  focused template-list selection / log scroll, same action as the
  arrow keys for whatever's focused (`tui::app::handle_mouse`). Click-
  to-focus/drag not implemented - not asked for, would need hit-testing
  against the actual rendered layout rects.

## Bug: starship + fish

- [ ] "starship init within fish doesn't work properly as is" — not yet
  investigated. Needs reproduction + root-causing before a fix; unrelated
  to the daemon/task-graph work, can land independently.

## Bug: `claude-update` intermittently fails the whole fixed sequence

- [x] **Root-caused, made non-blocking rather than "fixed" outright** -
  "usually fails, but worked on this VM's init" turned out to be real:
  `sudo claude update` failed with `sudo: claude: command not found`
  because there's no file literally named `claude` on PATH *for any
  user* on this beam, not a sudo-`secure_path`-specific issue (checked
  directly: plain `which claude` as this user comes back empty too).
  The actual npm-installed `claude` bin symlink exists, just moved aside
  to a hidden, randomly-suffixed name (e.g. `/usr/bin/.claude-<random>`)
  - and there's a *second*, differently-named, differently-versioned
  copy under `/usr/lib/node_modules/@anthropic-ai/` that's what any
  currently-running `claude` process (per `/proc/<pid>/exe`) actually
  runs from. Reads as deliberate harness/platform behavior - isolating
  the live session's own pinned copy from a `claude update` that could
  yank out from under it, and/or just not leaving an agent session a
  trivially-discoverable path to its own interpreter - not a bug in
  `steps/claude.rs` to route around by hunting down the hidden name
  (fragile across beams/sessions anyway, and uncomfortably close to
  defeating a boundary the platform put there on purpose).
  `steps/claude.rs` now checks `which claude` itself first and skips
  (`Ok(None)`) rather than erring when it comes back empty - this
  step's job is keeping a *separate*, human-facing `claude` current
  when one happens to exist by its normal name, not managing whatever
  the harness's own session is pinned to. A `claude` that *is* findable
  but genuinely fails to update still reports that failure as before.
  Verified live: the full (non-`--only`) sequence on this beam - where
  `claude` isn't resolvable right now - now exits `0` instead of
  stopping the `claude-update`/`start-bootstrap-daemon`/`install-
  teleport-bin` wave on an error.

## Repo delivery onto a beam

- [x] `tsh git ...` confirmed **not** viable for this (scoped to GitHub
  org access, not beam transport) — ruled out, don't revisit without new
  information.
- [x] **Implemented and verified live against a real, freshly spawned
  beam** (not just a design): push over git's `ext::` transport, run
  through the same `tsh ssh` channel the client already uses - see
  `client/src/tsh.rs`'s `prepare_git_target`/`push_repo` and docs/designs/DESIGN.md's
  "Repo delivery onto a beam" for the full design, including the
  branch-matching gotcha found while verifying this
  (`receive.denyCurrentBranch=updateInstead` silently no-ops instead of
  erroring if the beam's checked-out branch doesn't match what's
  pushed). Confirmed `git` *is* preinstalled on a genuinely fresh, stock
  beam - the blocking question below is resolved, no fallback needed.
  `client/src/stage.rs` now only stages the ssh key + bootstrap binary,
  down from staging the entire tracked tree.
- [ ] **New gap found during implementation**: a *reused* beam (tracked
  by `--purpose`, still within its TTL) never gets `push_repo` called
  again - only a freshly spawned one does. No explicit "re-sync this
  beam's repo" client command yet.
- [x] **Found and fixed**: the beam-side repo ended up with no `origin`
  remote at all - `git init` doesn't create one, and pushing over a bare
  `ext::` URL doesn't either (only `git clone` sets `origin`
  automatically). `prepare_git_target` now also points the beam's
  `origin` at `--repo`'s own real `origin` URL (`tsh::origin_url`, read
  from the workstation's local repo) - not the `ext::` URL itself, which
  only works from the workstation that ran the push. `None` (no local
  `origin`) just skips it, same as before this existed. Covered by new
  `client/src/tsh.rs` tests exercising the generated script against a
  real local git repo (first-call `remote add` and second-call
  `remote set-url` fallback both verified).

## TUI/UX polish, progress reporting, default startup clones

- [x] Zellij pane titles: the `tui` and log-tail panes in
  `shell/zellij/beam-shell.kdl` now have `name="bootstrap-tui"`/
  `name="bootstrap-log"` (the `ssh-agent` pane already had one) - the
  stacked-pane title bar no longer shows the raw `bash -lc ...` command.
- [x] TUI layout: `tui::app` is a single vertical column now (template
  list, gauge + task-progress list, status line) instead of a top row
  split 40/60 into a template-list pane and a side-by-side param-form
  pane - matches the sidebar's actual shape (tall, narrow). Param entry
  (`editing_params`) is a centered modal (`draw_param_modal`, ratatui's
  `Clear`-widget popup pattern) drawn over everything else instead of a
  permanent second pane; `Focus` dropped its `Params` variant since the
  modal already intercepts all input ahead of focus-based dispatch.
- [x] Mouse click on the template/progress lists: `handle_mouse` hit-
  tests a left click against `templates_area`/`progress_area` (the areas
  each list was last drawn into, border-excluded via a new `row_within`
  helper) and moves that list's selection to the clicked row, focusing
  that pane - same "select" action the scroll wheel already had, not
  "select and act" (opening the param modal still needs `Enter`).
  Ignored while the param modal is open, same as keyboard input.
- [x] Granular progress reporting: `tasks::Task`/`steps::Step` both
  gained a `run_with_progress(&self, progress: &dyn Fn(&str))` method,
  defaulting to ignore `progress` and delegate to the existing `run` -
  additive, no existing task/step needed to change. `executor::Event`
  gained a `Progress` variant; `executor::run` and `daemon.rs`'s
  `run_one_task` (the standalone-`run_task` path, which doesn't go
  through `executor::run` at all) both now call `run_with_progress`
  and broadcast/emit it as a new `task_status` `"progress"` status
  between `"start"` and the terminal one. `tui::app` shows it as a live
  message update on the task's row without changing its (already
  `Running`) state. `clone_git_repo::CloneGitRepo` is the one concrete
  task that overrides `run_with_progress` so far - runs `git clone
  --progress` through a new `releaseutil::run_streaming` helper (pipes
  stderr line-by-line to a callback, same contract as `run` otherwise)
  so each percent-complete line reaches the TUI/CLI live instead of only
  a single final message.
  - **Recommendation, not implemented**: `steps::teleport_bin`,
    `tasks::install_toolchain_go`/`install_code_server` (sizeable
    downloads - could report byte/percent progress the same way) and
    `steps::k3s_bot`/`tbot_service` (currently opaque single-message
    steps - could report "waiting for join token", "tbot starting",
    etc.) are the best next candidates now that the mechanism exists.
    Left as a recommendation rather than done here since each is also a
    product decision (stay a mandatory fixed `Step`, just with progress,
    vs. become an optional/deferred `Task` too) that needs picking per
    step, not a mechanical conversion.
- [x] Default startup clones: new `tasks::wait_ssh_agent_unlock`
  template (`wait-ssh-agent-unlock`, no params) polls `ssh-add -l` until
  `bin/ssh-agent-init`'s interactive unlock (still entirely outside this
  binary) has added the key, erroring out after a ~10 minute timeout
  rather than hanging forever; provides `tasks::cap::SSH_AGENT_UNLOCKED`.
  `clone_git_repo::CloneGitRepo::requires` now conditionally requires
  that capability for an ssh-style `repo` (`git@...`/`ssh://...`) -
  closing the gap its own doc comment used to flag. `bin/beam-shell`
  submits a manifest (`wait-ssh-agent-unlock` + the three repos below) to
  the already-running daemon in the background right after sourcing
  `ssh-agent-init`, so cloning starts without blocking reaching zellij,
  and is skippable via `BEAM_SHELL_SKIP_DEFAULT_CLONES=1` (for a future
  per-beam launcher that wants a different repo set):
  - `git@github.com:example-org/example-repo.git` → `~/projects/example-repo`
  - `git@github.com:timothyb89/teleport-harness.git` → `~/projects/teleport-harness`
  - `git@github.com:timothyb89/rdocker.git` → `~/projects/rdocker`

## Scheduling, default startup tasks, client wrappers

- [x] Dependency-driven scheduling replaces wave barriers (supersedes
  the wave-based items in "Migration to the daemon/task-graph design"
  above): `executor::run` starts each task as soon as its own
  `requires` have succeeded, rather than waiting for a whole wave to
  drain. A failure blocks only its (transitive) dependents, reported as a
  new `"blocked"` `task_status`. `Hub::run_lock` is gone, and
  `run_task`/`run_manifest` now run on their own thread after replying
  `accepted`, so neither the same connection (the TUI) nor other
  connections wait on an in-flight submission. Mutual exclusion now
  lives in a daemon-wide `executor::Resources` (exclusive resources +
  an implicit per-task-id lock). On the wire, `accepted` carries `tasks:
  [{task_id, depends_on}]` instead of `waves`, and `task_status` has no
  `wave` field any more.
  - [ ] No daemon-level test proves a second `run_task` on one
    connection runs while the first is still in flight (executor-level
    tests cover the scheduling itself). A test-only slow template, or
    an injectable task registry, would make that testable without
    relying on process-global env vars.
  - [ ] A manifest still can't depend on a capability provided by a task
    in a *different*, concurrently running submission - list the full
    chain in each manifest (the redundant entry skips/serializes via the
    per-task-id lock).
- [x] `bin/beam-shell`'s startup manifest now also includes
  `install-toolchain-go`/`install-toolchain-rust` by default
  (`BEAM_SHELL_SKIP_DEFAULT_TOOLCHAINS=1` opts out, alongside the
  existing `BEAM_SHELL_SKIP_DEFAULT_CLONES=1`).
- [x] `tasks::TemplateInfo::run_once`: marks no-param, startup-submitted
  setup templates (both toolchains, `wait-ssh-agent-unlock`); the TUI
  hides them from its template list. `install-code-server` is
  deliberately *not* marked, since nothing runs it at startup and hiding
  it would leave no TUI path to it at all.
- [x] TUI progress rows are two lines (status + task id, then the full
  message - or "waiting on ..." for a pending row, from the plan's
  `depends_on`); `[wN]` wave labels are gone with the waves themselves.
  New `Blocked` row state for the `"blocked"` status. Mouse hit-testing
  accounts for the two-line rows and the list's scroll offset (the
  latter was previously ignored, so clicks on a scrolled list picked the
  wrong row).

## k8s access workaround

- [ ] `k3s-bot-join` / `tbot` kube output exists purely because "beams k8s
  access doesn't work currently" per project.md. Periodically check
  whether native beams k8s access has been fixed upstream; if so, this
  whole workaround (bot-onboarding, `k3s_bot.rs`, `tbot_service.rs`,
  `kubeconfig.rs`) could potentially be dropped in favor of direct access.

## Testing

- [ ] `bootstrap` steps/tasks have no automated tests (see `AGENTS.md`).
  At minimum, consider extracting the pure logic (e.g. version/checksum
  parsing in `releaseutil.rs`, bot-join token/already-joined checks in
  `k3s_bot.rs`) into unit-testable pieces, separate from the
  side-effecting parts. Worth revisiting once the task-graph refactor
  lands, since dependency resolution itself is a good unit-test target.
- [ ] No CI configured (`.github/workflows` doesn't exist). At minimum a
  `cargo check`/`cargo test` workflow for both `bootstrap/` and `client/`
  would catch breakage before it's discovered on a live beam.
- [ ] Investigate intermittent missing stderr in
  `forward::tests::startup_failure_includes_captured_diagnostics`. During
  overlay validation it once reported only exit status 23; the isolated test
  and full-suite rerun passed.

## Client-owned services and remote requests

The design and phased implementation plan now live in
`docs/designs/CLIENT_SERVICES_DESIGN.md`, based on the transport/Mutagen findings in
`docs/designs/FOLDER_SYNC_SPIKE.md`. The governing boundary is that a Beam may submit a
typed request and observe status, but only the local `beams` client may approve
and own a folder sync, port forward, or file copy. These are deliberately not
bootstrap `Task`s.

- [x] Phase 1: `client/src/session.rs` replaces `beams ssh`'s direct `exec`
  into `tsh` with a supervised portable PTY. Normal input/output remains raw
  byte passthrough; local resize, child exit codes, Ctrl-C-as-input, and handled
  termination signals propagate; every exit path restores terminal raw/cursor
  state. `Ctrl-]` (configurable with `--escape-key`, including `none`) opens a
  local ratatui connection menu, and a second press sends the key
  remotely. The router also recognizes Kitty CSI-u and xterm modifyOtherKeys
  encodings of the configured key, as used when zellij enables enhanced
  keyboard reporting in terminals such as iTerm2, while preserving unmatched
  sequences byte-for-byte. Remote output is drained into a bounded 1 MiB
  newest-data buffer while the menu owns the screen, then restored before a
  two-step resize-driven remote redraw. Distinct resize notifications avoid
  coalescing, every local frame clears stale cells before repainting, and
  service forms render in a cleared modal. Daemon events repaint an open menu
  immediately so service/request status remains live. A new remote approval
  for this session's Beam preemptively opens the menu once, with a short guard
  that drops keys already buffered for the remote shell; other Beams' requests
  stay scoped to their own sessions, and `--escape-key none` remains strict
  passthrough. The menu deliberately clears the existing screen instead of
  nesting an alternate screen over zellij. Covered by pure byte-router/buffer
  tests and real PTY tests for
  binary input, exit status, and resize visibility, plus
  `client/tests/managed-session-smoke.sh` for the outer controlling-terminal
  path and rendered menu.
- [ ] Experimental trusted terminal overlay: `--terminal-overlay` keeps a
  `vt100` shadow screen in sync with passthrough output and paints bounded
  approval/service dialogs over it. The renderer is persistent and close no
  longer injects a synthetic resize pulse. Expand fixture/harness coverage
  for OSC title and clipboard requests, bracketed paste, mouse encodings,
  alternate-screen transitions, wide/combining characters, resize, and
  input/query response behavior before considering this path for default use.
  [2026-11-06 readiness audit](docs/development/terminal-overlay.md):
  expanded model/byte-router fixtures and a hermetic controlling-PTY harness
  cover the recommendations. Fixed pasted-prefix interception, mouse payload
  decoding, Kitty release/repeat toggles, title/clipboard and ordinary query
  handling, partial-sequence boundaries, active-screen/mouse restoration,
  inverse default colors, bisected wide toast glyphs, output starvation and
  ordinary mode cleanup on termination. Private CPR and common input-mode
  reports also use remote state. Saved-cursor and origin/margins acceptance
  gates now pass in the ordinary suite; the DCS query gate remains ignored
  and failing. Keep opt-in.
  Also resolve unknown initial/inactive screen state,
  unsupported drawing/input protocols, and independent
  workstation font/reflow/lifecycle validation before changing the default.
  The [2026-11-06 handoff](docs/designs/TERMINAL_OVERLAY_DESIGN.md#implementation-status-and-next-session-handoff)
  records completed commits, validation and implementation entry points.
  Next: DECRQSS replies from remote state, then child-input backpressure;
  the saved-cursor and origin/margins fixes do not complete compatibility work.
  Follow the [resolution plan](docs/designs/TERMINAL_OVERLAY_DESIGN.md) in
  separately reviewable milestones:

  - [x] First small slice: `beams fetch` logs accepted requests as a lasting
    stderr acknowledgement, including when a toast appears, using the existing
    return path. Generic guidance directs the user to the workstation's Beams
    menu; session-specific prefix/availability hints remain optional future
    work rather than assuming the default key or adding a notice protocol.
  - [x] Model/backend foundation: a focused vendored `vt100` 0.18.1 extension
    exposes current/saved cursors and attributes, both buffers' origin/margins,
    and streaming DCS callbacks. The shared `terminal_stream` framer bounds
    control sequences at 64 KiB before allocating parser input; overflow closes
    an overlay and resumes original output without truncation. Painting stays
    suppressed until observed `ESC c`. Event count/bytes are bounded; the live
    relay drains each frame, including 512-query bursts. Silence never implies
    a sequence terminator. Fixtures and the controlling-PTY harness cover this.
  - [ ] Complete snapshot/eligibility coverage for further drawing/input state
    and content provenance. The foundation's loss flag handles byte-budget
    exhaustion, not unknown initial cells or every unsupported protocol.
  - [x] Shared painting for represented saved/current cursors, SGR attributes,
    origin mode and vertical margins. Menus and toasts draw under full margins
    with origin off; close restores per-buffer saved slots, including hidden
    updates. Toasts never borrow a save slot. DEC and xterm-style ANSI aliases
    are covered; saved-cursor and origin/margins gates are unignored.
    Takeover replays deferred bytes from its captured entry state.
  - [ ] Extend painting to insert mode, DEC character sets, OSC 8 hyperlinks
    and every touched inactive buffer, with content provenance and resize
    invalidation. A limited xterm.js 5.5 replay validates nine restoration
    traces and the actual menu renderer, but saved-wrap/origin semantics differ
    from the model. Establish the supported terminal behavior or defer painting
    on those variants before changing the default.
  - [ ] Ordered remote/physical query dispatch, starting with DECRQSS and
    origin-relative CPR. Unignore the DCS acceptance gate; test exactly-once
    replies and bounded query pressure. Origin-relative CPR is implemented;
    audit child-input backpressure (reply writes are still synchronous).
  - [ ] Content provenance and unsupported-feature policy; settle compatibility
    UX before integrating it. Proposed default: defer unsafe painting, retain
    explicit best-effort takeover. Cover unknown inherited screens, common
    drawing protocols, resize/reflow invalidation and mid-dialog fallback.
  - [ ] Guarded alternate-buffer menu fallback for unknown primary screens:
    preserve primary content and cursor/mode state, establish inactive-buffer
    ownership, close/replay before output overflow or a remote buffer switch,
    and deliver queries/effects exactly once. Never nest over a remote-owned
    alternate buffer or switch screens for ordinary toasts.
  - [ ] Optional richer inline approval guidance for uncertain terminal states,
    skippable if high effort. Correlate the request's originating session with
    frontend prefix/UI availability; add capability-negotiated advisory notices
    and render them in `beams fetch`/other waiting CLIs without changing approval
    status. Test custom/disabled prefixes, multiple frontends and old peers.
    This enhancement does not block the overlay fixes/default-readiness work.
  - [ ] Session-entry/exit state ownership and advanced keyboard/mode cleanup,
    including non-default outer-terminal state and partial-string termination.
  - [ ] Independent emulator comparison and recorded workstation matrix with
    Zellij, plain shell, pager/editor, fonts/reflow and lifecycle checks.
  - [ ] Separate default switch after all gates pass, retaining an explicit
    takeover option and strict `--escape-key none` behavior.

- [x] Local menu overhaul: one renderer (`client/src/session/ui.rs`) for both
  takeover and overlay modes (the overlay previously had a cut-down copy that
  hid most copy/sync form fields and printed services with `{:?}`); a
  generic form model (`session/form.rs`: text/number/toggle/choice fields,
  focus, per-field errors) replacing five ad-hoc `Option<...Form>` structs;
  real menu key decoding (`session/keys.rs`: arrows, Shift-Tab, Home/End,
  Delete, readline chords, UTF-8, SS3 cursor keys, Kitty/modifyOtherKeys,
  bracketed paste); a line editor with a visible cursor, horizontal
  scrolling, and fish-style workstation path completion. Service selection
  is clamped consistently for display and actions.
- [x] Bridges to deleted Beams: the daemon kept one reconnecting bridge per
  Beam for its whole lifetime, and reset the backoff whenever `tsh` merely
  spawned, so each deleted Beam respawned `tsh ssh` every ~250ms forever.
  The local menu also showed every Beam's bridge status in its own status
  line. The backoff now resets only after a real hello. Bridges failing for
  20s are retired once `tsh beams ls` confirms the Beam is gone, which also
  stops that Beam's forwards and copies and pauses its syncs. The menu
  filters bridge status to its own Beam.
- [x] Daemon lifecycle: `beams daemon status|stop|restart` over
  version-independent control messages (SIGTERM via the socket peer PID for
  older daemons). The daemon self-restarts into a replaced binary once it is
  stable and idle, handing over the listening socket and restoring forwards.
  `beams ssh` replaces a protocol-incompatible daemon unless it is itself
  stale.
- [ ] Daemon updates across install paths: self-restart only watches the
  daemon's own executable path. A daemon started from `~/.cargo/bin/beams`
  ignores a newer `target/debug/beams` (`beams daemon status` points this
  out; `beams daemon restart` from the new binary switches it). A cross-path
  "newer build wins" rule would need an orderable build stamp.
- [x] Local menu home screen: one selectable list of every pending request
  (oldest first) plus services, with column headers. Selection tracks row
  identity. `Enter` opens a request decision menu or a service's details and
  state-appropriate actions. Terminate/cancel ask first, defaulting to "Keep
  it". A takeover selects the new request.
- [x] Workstation path picker: path fields get a fuzzy-filtered dropdown of
  the typed directory (Tab/arrows/Enter, opens upward or shrinks on short
  terminals) and offer recently used directories while empty.
- [x] `beams fetch PATH... [DEST]` on the Beam (`bin/beams` runs
  beam-bootstrap as `beams`): a capability-gated multi-file `file_fetch`
  request with sizes, a "Save to" approval form, per-item `tsh scp` into temp
  names, byte progress back to the Beam, and withdraw-on-Ctrl-C.
- [ ] Faster fetches of many files: stream `tar -cf - -C <parent> <names>`
  over one `tsh ssh` into a local `tar -x` in the temp directory (counting
  piped bytes for progress) instead of one `tsh scp` per source. One
  connection for globs, and no per-file SFTP round trips through the proxy
  for directory trees. Keep scp for a single plain file. Later options:
  ControlMaster connection reuse, one-off Mutagen syncs.
- [ ] A workstation daemon crash mid-fetch can leave a hidden
  `.beams-fetch-*.part` in the destination; clean these up when an
  interrupted copy is noticed at startup.
- [ ] Some client unit tests remove their `/tmp/beams-*` scratch dirs only
  on success; use a drop guard so failures don't leak them.
- [x] Non-blocking toasts: a bottom-right notice drawn over live remote
  output (painted only between escape sequences, repainted after output
  bursts, cells restored from the shadow model) for incoming requests and
  transfer progress, instead of an input takeover; `Ctrl-]` opens straight
  into the request and returns to the session after. Verified live over
  zellij and `less`. (`--approval-takeover`, the old open-on-arrival
  behavior, was removed 2026-11-06: toasts work in both display modes.)
- [x] Phase 2: `beams daemon` is the single-instance per-user workstation
  daemon, auto-started by `beams ssh` (with `--no-client-daemon` as a
  troubleshooting escape hatch). `client/src/clientd.rs` provides a mode-0600
  UDS, typed/versioned NDJSON handshake and events, 64 KiB frame and bounded
  event-queue limits, per-connection rate limiting, handshake timeout, stale-
  socket handling, explicit blocking mode for accepted sockets on BSD/macOS,
  and frontend subscriptions/snapshots. Its atomically replaced mode-0600 state
  file owns a stable UUID `client_id` plus typed request/service records and
  explicit transition tables. The Phase 1 menu now shows the live client ID,
  frontend count, pending-request count, service count, and daemon
  disconnect/errors. Unit tests exercise protocol framing, transitions, state
  durability/permissions, socket ownership, rate limiting, and a real
  `UnixStream` handshake (including an inherited nonblocking stream); the
  controlling-terminal smoke test also runs a real temporary daemon and
  verifies its client ID is rendered.
- [x] Phase 3: add `beam-bootstrap client-bridge`, VM-daemon client/service
  projection, managed-session routing tokens, bridge reconnect/snapshots, and
  remote CLI requests. Direct or stale unmanaged sessions must fail clearly.
  - [x] Shared `protocol/` crate plus the VM half: typed, versioned bridge
    messages; `beam-bootstrap client-bridge`; daemon bridge registration,
    exact session-token routing, request/service projections and status
    broadcasts; and `beam-bootstrap client list` /
    `client request port-forward`. Tests cover stable serialization,
    one-of-many routing, rejection transitions, and direct/stale-session
    errors. The request deliberately creates no forward yet.
  - [x] Workstation half: supervise one reconnecting bridge per Beam, register
    each managed shell token, show/decide pending requests locally, expire
    unanswered requests, and resend snapshots after either daemon restarts.
    A bridge stays unroutable until its restart snapshots have crossed an
    explicit `ready` barrier. `client-bridge-smoke.sh` covers two client
    daemons, exact-token routing and rejection, plus VM/local daemon restarts;
    the same request/rejection path was also exercised through real `tsh ssh`
    against disposable Beam `outer-aura`, which was removed immediately
    afterward. Phase 4 will turn approval into a real loopback listener.
- [x] Phase 4: implement loopback-only local port forwards as the first full
  client- and remote-initiated service.
  - [x] Workstation runtime: client- and remote-initiated requests share one
    creation path; `beamsd` supervises loopback-only `tsh ssh -N -L` children,
    auto-selects or preflights local ports, captures bounded diagnostics, and
    exposes persistent typed status plus local stop/restart controls.
  - [x] The remote TUI now has `Tasks | Services | Requests` tabs. Service
    stop/restart keys submit typed action proposals routed only to the owning
    client and require a fresh local approval. Full-state and reconnect
    snapshots cover requests, action requests, and services.
  - [x] Tests cover two local frontends racing one decision, two clients on one
    Beam without cross-client control, bind conflicts, diagnostics,
    stop/restart, parent-daemon death, VM/local daemon restart, and selected
    port recovery. `client-bridge-smoke.sh` runs the complete lifecycle with a
    real loopback listener. A final disposable-Beam test (`stable-outpost`,
    removed immediately afterward) used real `tsh ssh -N -L`: remote approval
    exposed the Beam's HTTP server on `127.0.0.1:44693`, stop closed it, and
    restart restored the same endpoint.
- [x] Phase 5: add typed client↔Beam file-copy jobs with locally selected or
  confirmed workstation paths, overwrite policy, cancellation, and interrupted
  job handling.
  - [x] Client- and remote-initiated upload/download requests share one
    workstation-owned one-shot `tsh scp` runtime. The local menu provides
    upload/download forms and confirms the real local path for remote
    proposals; `beams copy-to`/`copy-from` provide blocking CLI equivalents.
  - [x] Workstation paths remain local-only. The shared protocol carries only
    a display hint, and service projections redact detailed local paths/errors.
    Tests caught and guard this boundary.
  - [x] Deny-overwrite is the default, with sibling temporary files and atomic
    no-clobber/replace installation. Cancellation removes partial files
    best-effort; copy jobs cannot restart, and daemon restart marks an active
    job interrupted instead of replaying it.
  - [x] Unit tests cover success, diagnostics, overwrite denial, cancellation,
    local-path authority, and restart semantics. `client-bridge-smoke.sh`
    covers a remote-initiated download through both real daemons and verifies
    that its workstation path is absent from remote status.
- [ ] Phase 6: productize the Mutagen wrapper transport and add folder-sync
  lifecycle/status/conflicts/recovery. Default to `--ignore-vcs` and
  `two-way-safe`; destructive replacement and local-wins behavior stay
  explicit.
  - [x] Typed remote requests carry the Beam path, display-only local hint,
    mode, VCS-ignore setting, and ignore rules without granting the Beam an
    authoritative workstation path. Both local TUIs show lifecycle/conflict
    state and route pause/resume/terminate proposals through local approval.
  - [x] `beamsd` owns a private Mutagen data directory and mode-0700
    `tsh`-dispatching wrappers, checks for Mutagen 0.18+, persists a
    deterministic session name, validates both endpoints/settings when
    adopting, and detaches rather than terminating on daemon shutdown.
    Client creation/control is available from the hotkey menu and
    `beams sync create/list/pause/resume/terminate`.
  - [x] A real disposable-Beam run from this ephemeral Beam (`hidden-lab`,
    removed immediately afterward) covered pre-populated-directory merge,
    propagation in both directions, pause/resume isolation, `two-way-safe`
    conflict reporting, daemon-restart adoption, CLI conflict detail, and
    explicit termination removing the Mutagen session. Unit tests cover
    transport permissions, deterministic naming, endpoint drift, status
    summaries, missing-Mutagen diagnostics, restart persistence, and
    local-path/error redaction.
  - [ ] Run the final laptop-to-Beam acceptance check (including macOS) and
    add an automated fake-Mutagen fixture for disconnected transport and
    expired-Beam regression coverage.
- [ ] Phase 7: run the isolated OpenSSH `ControlMaster`/`tsh proxy ssh`
  compatibility spike and add a multiplexed backend only if it passes
  Teleport auth renewal, MFA, recording, reconnect, PTY, forward, copy, and
  Mutagen tests. Keep separate long-lived `tsh` processes as the baseline;
  reconsider `russh` only after profiling identifies a real need.
  - [x] Record the process/credential findings, proposed daemon-owned master
    design, and fallback rules in `docs/designs/OPENSSH_MULTIPLEXING_SPIKE.md`. Add
    `client/tests/openssh-controlmaster-spike.sh` for command, PTY,
    concurrency, SCP, dynamic-forward/cancel, Mutagen-style options, and
    master-recreation checks. This Beam's ProxyCommand and host-CA path work,
    but its tbot/PIV identities cannot authenticate OpenSSH as `beams`.
  - [ ] Run the harness from macOS and Linux workstation profiles, then test
    Teleport per-session MFA, recording/audit behavior, certificate expiry and
    `tsh login` recovery, network loss, Beam expiry, real Mutagen, and the
    foreground interactive PTY before implementing or enabling
    `OpenSshTransport`.

## SSH agent proxy (see `docs/designs/SSH_AGENT_PROXY_DESIGN.md`)

No private key goes to a Beam anymore: the Beam's `SSH_AUTH_SOCK` is
`beam-bootstrapd`'s relay, and each signature is approved in the attached
`beams ssh` (Allow once / Deny, Deny preselected), signed by that
terminal's own agent. Startup clones can be covered by a scoped startup
grant. Keys are opted in with `beams agent-keys --allow N`, the one-time
offer toast, or `a` in the menu. Phases 0-3 and most of phase 4 landed on
2026-11-04 (commits `0f3e664`..`f618adf`); the design doc's "Handoff notes"
lists the code map, tests, and gotchas. 2026-11-06 closed the small
follow-ups: run tags on every git task, routing a manifest's key use to
its submitting client, the `ssh-agent-audit.jsonl` audit log, task names
in grant-use toasts, pruning retired Beams from `startup_submitted`, a
home-screen key-setup hint, a typing-proof review input guard, and
removing `--approval-takeover` (it let stray typing open and approve an
SSH key review; requests only ever toast now). Pre-switch
Beams (staged key, old CLAUDE.md merge) have all aged out (24h TTL).

Open:

- [ ] Remembered approval rules ("allow github.com as git for 30m"),
  scoped to Beam + key + verified host key + user, expiring no later than
  the Beam's TTL, listed and revocable like startup grants. Never matching
  an unverified host. (Design: "Grants and remembered rules".)
- [ ] A GUI/OS approval surface (macOS notification or dialog) for when no
  `beams ssh` is attached. Today such requests are denied without asking,
  by design. The design's `ApprovalSurface` seam isn't factored out yet:
  the terminal flow lives in `clientd.rs` (`handle_agent_sign`,
  `decide_agent`).
- [ ] Host certificates (`*-cert-v01@openssh.com` host keys,
  `@cert-authority` lines) aren't supported: such hosts show "verified key,
  unknown host" and can't match a grant.
- [ ] The hidden `beam-bootstrap agent-proxy-spike` subcommand
  (`bootstrap/src/agentspike.rs`) is kept for
  `bootstrap/tests/ssh-agent-proxy-spike.sh` and `ssh-agent-hostkey-algos.sh`.
  Remove it if those scripts get rebased onto the real relay.

## Explicitly deferred — needs its own design pass later, not blocked on
anything above

- [x] **Fixed, see the "New tasks enabled by the graph" section below** -
  web-based editor hosting via a new `install-code-server` task.
- [ ] `tsh beams publish` (HTTP/TCP app publishing from a beam) isn't
  integrated into either binary yet — now actually relevant, since
  `install-code-server` (see the "New tasks enabled by the graph"
  section above) gives it something real to publish. Still a manual
  step today (`tsh beams publish $BEAM_ALIAS`), not automated by either
  binary.
  - **Known issue, root cause not confirmed**: code-server's websocket
    connection fails (browser console: `WebSocket close with status
    code 1006`) when reached through a published HTTP app, even though
    the initial page load works. Ruled out: code-server itself (a local
    `curl -i -N -H "Connection: Upgrade" -H "Upgrade: websocket" ...`
    straight to `127.0.0.1:8080` gets a clean `101 Switching Protocols`
    on this beam); same-origin/CORS (the wss:// URL the page itself
    generates already correctly uses the published app's own hostname,
    computed server-side from the forwarded `Host` header - so the
    "random suffix" in the app name isn't actually a config-coordination
    problem, there's no separate allow-list to keep in sync with it);
    HTTP/2-vs-websocket at the browser-facing edge (`curl -v` against
    the public app address shows ALPN negotiating plain `http/1.1`, not
    `h2`, so it's not the classic "h2 doesn't support Upgrade: websocket"
    failure mode at that hop). `tsh apps ls --format json` shows the
    published app's backend `uri` is `https://<this beam's own eth0
    IP>:8443` with `tls.mode: verify-spiffe-id` - i.e. `tsh beams
    publish` doesn't point Teleport at `127.0.0.1:8080` directly; there's
    a root-owned ingress agent already listening on the beam (ports
    8443/8444, outside this repo's code entirely - part of the beam
    image) that mTLS-terminates via SPIFFE workload identity and
    forwards to the real local port. Best remaining suspect: that
    Teleport-proxy-to-beam-ingress hop (likely HTTP/2/gRPC-based, given
    the mTLS/workload-identity setup) not tunneling a websocket Upgrade
    across it - couldn't confirm directly, since this session's
    delegated identity gets `access denied: identity is not allowed to
    reissue certificates` on `tsh apps login` (can't drive the same path
    a real browser would from here). Next step: try `tsh beams publish
    --tcp` instead (raw TCP passthrough, no HTTP-aware reverse proxying)
    plus `tsh proxy app` locally as a workaround/isolation test - from a
    real (non-delegated) `tsh login` session, not this beam's own.
- [x] `beams ls`/`rm`/`exec`/`scp`/`publish`/`unpublish`/`ide` wrap
  `tsh beams ...`, accepting a beam id, uuid, or tracked purpose
  (`client/src/beamcmd.rs`). `ls` shows purposes + time left and flags
  stale purposes; `rm` forgets purposes for removed beams, `--prune`
  forgets ones whose beam is already gone.
- [ ] Still no proactive warning when a tracked purpose's beam is close
  to its TTL (beyond `beams ls` showing time left).
- [ ] Confirm whether `teleport-harness` (the actual consumer of the k3s
  workaround, per project.md) has been exercised end-to-end from a beam
  yet, or is still just a theoretical target.
