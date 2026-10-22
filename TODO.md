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
  - **New gap, found restarting the daemon mid-E2E-test**: the TUI
    doesn't reconnect if its daemon connection drops (e.g. `beamctl
    restart beam-bootstrapd` after swapping the binary, which is what
    happened here) - it just goes quiet, showing stale state forever.
    Not a normal occurrence (daemon restarts aren't routine), but worth
    a reconnect loop in `tui::client::DaemonClient` eventually.
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
- [ ] **New gap found while adding these**: `Task::exclusive_resources`
  only serializes tasks within *one* `executor::run` call - it's a
  `Mutex` local to that call, not a cross-process lock. The fixed
  startup sequence (`apt-install`) and a later daemon-driven manifest
  (`install-toolchain-rust`'s `apt-get install gcc`) are two genuinely
  separate OS processes with no shared lock at all, so they *could*
  still race on the real apt/dpkg lock if a manifest happened to be
  submitted while the startup sequence's own `apt-install` wave was
  still running. Low-probability in practice (the fixed sequence
  usually finishes within a minute or two of a beam spawning, before
  anyone's submitted a manifest), but a real gap, not a solved problem -
  would need a cross-process lock (e.g. a well-known flock file) to
  close properly. Not fixed here.

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

## Repo delivery onto a beam

- [x] `tsh git ...` confirmed **not** viable for this (scoped to GitHub
  org access, not beam transport) — ruled out, don't revisit without new
  information.
- [x] **Implemented and verified live against a real, freshly spawned
  beam** (not just a design): push over git's `ext::` transport, run
  through the same `tsh ssh` channel the client already uses - see
  `client/src/tsh.rs`'s `prepare_git_target`/`push_repo` and DESIGN.md's
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
