# TODO

Live task list. See `DESIGN.md` for the current architecture direction
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

## Done so far (for context, not actionable)

- `beam-bootstrap`: dotfile linking, apt packages, fish/starship,
  kubectl/zellij install, k3s-bot join + `tbot` as a background service,
  kubeconfig symlink, `claude update`. Idempotent, JSON or human output.
  Current implementation: a fixed, run-once step list — see "Migration"
  below for how this becomes the daemon/task-graph's zero-param
  templates.
- `beams` client: spawn-or-reuse by purpose, concurrency cap, staging +
  scp of repo + prebuilt bootstrap binary, `tsh ssh` into `beam-shell`.
- `bin/beam-shell`: zellij layout with main pane, bootstrap log tail, and
  an async ssh-agent-unlock pane that never blocks a noninteractive run.
- `AGENTS.md`, `DESIGN.md`: architecture/conventions + the
  daemon/task-graph design written up and agreed.

## Migration to the daemon/task-graph design (see DESIGN.md)

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
- [x] **Concurrent execution** (see DESIGN.md's "Concurrent execution"):
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
  stayed with the "boring" approach DESIGN.md calls for; a handful of
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
- [ ] CLI/TUI sugar for `run_manifest` is still missing - `run-task`
  (the CLI) only calls the single-task `run_task` op, not
  `run_manifest`; there's no CLI way yet to hand the daemon a multi-task
  manifest without hand-writing the JSON (as the README's example does).
  Worth deciding whether that's a `run-manifest --file <path>` flag, or
  waits for the TUI/`client` wiring below to be the real consumer.
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
- [ ] `beams task run <template> [params...]` — agent-friendly on-demand
  CLI entry point, no TUI required. (Distinct from the item above: this
  is a *daemon-backed* CLI, talking over the socket like the TUI would,
  vs. today's `run-task`/`run_manifest`-via-flag question, which is
  in-process-vs-socket-client more than daemon-vs-no-daemon - may end up
  being the same command once the daemon is what's actually running
  things day to day.)
- [ ] Wire manifest seeding into `client`'s `beams ssh`/`beams new` (e.g.
  `beams new --clone <pr>` builds a manifest and hands it to a freshly
  spawned beam before its first startup pass).

## New tasks enabled by the graph (deferred-by-default templates)

`clone-git-repo`/`checkout-pr-worktree` landed — see the "Migration"
section above for status/details. Still open:

- [ ] `install-toolchain-go`, `install-toolchain-rust`.

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
- [ ] `beam-bootstrap tui` (now built, see "Migration" below) still
  needs to actually be spawned into a beam-shell session - the plan is
  imperatively (`zellij action new-pane --direction right`) into an
  already-open session rather than baked into the layout file, so it
  doesn't hit the same per-tab-replay problem this fix addressed. Not
  wired into `bin/beam-shell` yet.

## Bug: starship + fish

- [ ] "starship init within fish doesn't work properly as is" — not yet
  investigated. Needs reproduction + root-causing before a fix; unrelated
  to the daemon/task-graph work, can land independently.

## Repo delivery onto a beam

- [ ] `tsh git ...` confirmed **not** viable for this (scoped to GitHub
  org access, not beam transport) — ruled out, don't revisit without new
  information.
- [ ] Chosen direction: push over the same ssh transport the client
  already uses to reach the beam (`tsh ssh`'s cert auth), not the repo's
  dedicated keypair. See DESIGN.md's "Repo delivery onto a beam" for the
  full design (non-bare repo on the beam with
  `receive.denyCurrentBranch=updateInstead`, client adds it as a git
  remote and pushes).
- [ ] **Blocking question to resolve before implementing**: is `git`
  preinstalled on a stock beam? If not, it needs to ride in via the
  existing scp step (same as the prebuilt binary) before the first push
  can land. Keep today's `git ls-files` + scp delivery working as a
  fallback until this is confirmed and the push path is proven.
- [ ] Once confirmed: switch `client/src/stage.rs` from
  content-copying tracked files to the push-based transport.

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

## Explicitly deferred — needs its own design pass later, not blocked on
anything above

- [ ] Web-based editor hosting (`code-server`/`openvscode-server` vs.
  lighter terminal-based options like `ttyd` + neovim/helix) — see
  DESIGN.md.
- [ ] Client↔beam folder sharing (mutagen-style, e.g. for
  `~/projects/core`) — see DESIGN.md.
- [ ] `russh`-based data channel / on-the-fly port-forward management —
  see DESIGN.md. Start with shelling out to `tsh proxy ssh`/`tsh ssh -L`
  instead.
- [ ] `tsh beams publish` (HTTP/TCP app publishing from a beam) isn't
  integrated into either binary yet — likely becomes relevant once the
  web-editor-hosting item above is active.
- [ ] No cleanup/expiry helper on the client side beyond the 5-beam cap
  check at spawn time — e.g. a `beams ls`/`beams rm` convenience wrapper
  around `tsh beams ls/rm`, or a warning when a tracked purpose's beam is
  close to its TTL even if not actively reused.
- [ ] Confirm whether `teleport-harness` (the actual consumer of the k3s
  workaround, per project.md) has been exercised end-to-end from a beam
  yet, or is still just a theoretical target.
