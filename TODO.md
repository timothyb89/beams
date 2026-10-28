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
  - `git@github.com:gravitational/core.git` → `~/projects/core`
  - `git@github.com:timothyb89/teleport-harness.git` → `~/projects/teleport-harness`
  - `git@github.com:timothyb89/rdocker.git` → `~/projects/rdocker`

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

## Client-owned services and remote requests

The design and phased implementation plan now live in
`CLIENT_SERVICES_DESIGN.md`, based on the transport/Mutagen findings in
`FOLDER_SYNC_SPIKE.md`. The governing boundary is that a Beam may submit a
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
  immediately so service/request status remains live. The menu deliberately
  clears the existing screen instead of nesting an alternate screen over
  zellij. Covered by pure byte-router/buffer tests and real PTY tests for
  binary input, exit status, and resize visibility, plus
  `client/tests/managed-session-smoke.sh` for the outer controlling-terminal
  path and rendered menu.
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
- [ ] Phase 5: add typed client↔Beam file-copy jobs with locally selected or
  confirmed workstation paths, overwrite policy, cancellation, and interrupted
  job handling.
- [ ] Phase 6: productize the Mutagen wrapper transport and add folder-sync
  lifecycle/status/conflicts/recovery. Default to `--ignore-vcs` and
  `two-way-safe`; destructive replacement and local-wins behavior stay
  explicit.
- [ ] Phase 7: run the isolated OpenSSH `ControlMaster`/`tsh proxy ssh`
  compatibility spike and add a multiplexed backend only if it passes
  Teleport auth renewal, MFA, recording, reconnect, PTY, forward, copy, and
  Mutagen tests. Keep separate long-lived `tsh` processes as the baseline;
  reconsider `russh` only after profiling identifies a real need.

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
- [ ] No cleanup/expiry helper on the client side beyond the 5-beam cap
  check at spawn time — e.g. a `beams ls`/`beams rm` convenience wrapper
  around `tsh beams ls/rm`, or a warning when a tracked purpose's beam is
  close to its TTL even if not actively reused.
- [ ] Confirm whether `teleport-harness` (the actual consumer of the k3s
  workaround, per project.md) has been exercised end-to-end from a beam
  yet, or is still just a theoretical target.
