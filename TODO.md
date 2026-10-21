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
- [ ] **Known gap**: `graph::resolve_order<N: Node>` is monomorphic over
  one `N` — a `Vec<Box<dyn Step>>` and a `Vec<Box<dyn Task>>` can each be
  resolved on their own (as `steps::resolved()` already does), but can't
  be resolved *together* in one call as-is. The daemon's actual task
  graph (mixing fixed startup steps and templated instances, e.g.
  `checkout-pr-worktree` depending on a `clone-git-repo` instance) will
  need a single node representation spanning both — worth settling as
  part of the daemon skeleton below, not before (no concrete need yet to
  design against).
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
- [ ] `beam-bootstrap tui`: ratatui client, connects to the daemon's
  socket, deferred/templated task list + param input on top (building a
  manifest from the selection), progress bar + scrollable log on bottom
  (rendering the `task_status`/`manifest_status` broadcasts already
  implemented).
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

- [ ] Fix `shell/zellij/beam-shell.kdl` so its tab template only applies
  to the *first* tab of a session, not every new tab — currently every
  new tab re-shows the bootstrap-log-tail pane and an ssh-agent-unlock
  pane that just exits immediately (key already unlocked by then). See
  DESIGN.md's "zellij layout fix".
- [ ] The future bootstrap-tui pane should be spawned imperatively
  (`zellij action new-pane --direction right`) into an already-open
  session rather than baked into the layout file, so it doesn't hit the
  same problem.

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
