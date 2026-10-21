# DESIGN.md — bootstrap daemon + task graph

This supersedes the "fixed step list that runs once and exits" design for
`beam-bootstrap` described in `bootstrap/README.md`. That README still
describes what's *implemented today*; this file describes where it's
going. `project.md` is the original brief and stays as historical context.
Update `TODO.md` as pieces of this land.

## Why

The original design (see `project.md`) only covered unattended startup
tasks. Two things it didn't anticipate:

- Not everything should run automatically on startup — some tasks are
  expensive (cloning a large repo, installing a toolchain) and should be
  offered, not forced, and some need user-supplied parameters (which PR,
  which path) that aren't known until someone asks for them.
- The same task ("clone this repo", "check out this PR") needs to run
  both as part of initial beam setup *and* on demand later, from a TUI, a
  CLI, or eventually a GUI — so it needs to exist as one reusable
  definition, not three different ad-hoc implementations.

That turns `beam-bootstrap` from a run-once script into a long-lived
daemon that owns a graph of tasks, reachable over a local socket by
whatever UI is asking for something.

## Architecture

```
                 UDS socket (~/.beam-bootstrap/daemon.sock)
                           |
      +--------------------+--------------------+
      |                    |                    |
  beam-bootstrap tui   beams task run ...   (future: Mac app,
  (ratatui, zellij      (CLI, scriptable,     tunneled over a
   side panel)           agent-friendly)       port-forward)
```

`beam-bootstrap` becomes two things in one binary:

- `beam-bootstrap daemon` — the long-running process. `beamctl start
  beam-bootstrapd -- beam-bootstrap daemon` is how it gets spawned
  ad-hoc, same mechanism already used for `tbot`. Owns the task graph,
  runs tasks, listens on the socket.
- `beam-bootstrap tui` — a ratatui client that connects to the daemon's
  socket. Doesn't run tasks itself; it's a view onto the daemon plus a
  form for triggering deferred/templated tasks.

Today's one-shot binary behavior (`beam-bootstrap --repo ...`, run from
`bin/beam-shell`) becomes: ensure the daemon is running, then it's the
daemon — not the CLI invocation — that actually runs the startup tasks.
`--only <step-id>` and `--json` remain useful as thin CLI wrappers around
the same socket protocol, for scripts that don't want a daemon in the
loop (`bin/beam-shell`'s synchronous `link-files` pre-step is a candidate
to just keep working this way, since it has to run before the daemon can
even be reached in the first place).

## Wire protocol

Newline-delimited JSON over the UDS socket. No framing beyond `\n`, no
schema negotiation — kept deliberately boring so any future client
(including a Swift Mac app, if the socket is ever tunneled out over a
port-forward) can speak it without a generated-client toolchain. Extends
the JSON-lines event shape `bootstrap/src/events.rs` already emits.

- Every client request carries a `request_id`; every daemon response
  (direct reply or an async event caused by that request) echoes it, so a
  single connection can have several things in flight.
- Task progress/status events are broadcast to *every* connected client,
  not just the one that triggered the task — this is what lets the TUI
  show progress for a task someone else kicked off from the CLI, per the
  "on-demand tasks report progress to the tui" requirement.
- Rough request shapes (illustrative, not final):
  ```json
  {"request_id": "...", "op": "list_tasks"}
  {"request_id": "...", "op": "run_task", "task": "clone-git-repo", "params": {"repo": "...", "pr": 1234, "dest": "~/projects/core"}}
  {"request_id": "...", "op": "subscribe"}
  ```
  and events:
  ```json
  {"type": "task_status", "task_id": "clone-git-repo:dest=~/projects/core", "status": "running", "progress": {"message": "..."}}
  ```

## Task model

### Capabilities, not task IDs, for dependencies

A task declares the capabilities (named, string-keyed facts) it
**provides** and the capabilities it **requires**. The scheduler topo-sorts
on capabilities, not on fixed task IDs:

- `ssh-agent-init` provides `ssh-agent-unlocked`.
- `clone-git-repo` (templated, see below), when instantiated with
  `dest=~/projects/core`, provides `repo-cloned:~/projects/core`.
- A `checkout-pr-worktree` instance requires `repo-cloned:<base_path>` —
  it doesn't care *which* task produced that clone, just that the
  capability exists before it runs.

This is the piece that makes the design scale to templated/instanced
tasks: a fixed task-ID dependency can't be named before a template is
instantiated with concrete params, but a capability can be named up
front by any task that happens to need it. It also means the existing
idempotency pattern (steps probe real state rather than trust a run log)
carries over unchanged: "does this capability already hold" is answered
the same way "should this step skip" is answered today — by checking the
filesystem/package state directly, not a cache.

### Templates vs. instances

- A **template** is a task definition with a param schema (e.g.
  `clone-git-repo(repo: string, pr: Option<u32>, dest: path)`).
- **Instantiating** a template with concrete params produces a task
  instance with a derived ID (`clone-git-repo:dest=~/projects/core`) that
  participates in the dependency graph like any other task.
- Today's fixed steps (apt-install, install-kubectl, …) become
  zero-param templates, auto-instantiated once at daemon startup — no
  behavior change for them, just a reframing.

### Deferred / optional tasks

A template can be marked `deferred: true` — the daemon registers it (so
the TUI/CLI can see it and its param schema) but does not auto-run it.
It only becomes a task instance when something explicitly instantiates
it: a TUI form submission, a CLI `run_task` call, or a manifest entry
supplied at beam-spawn time (see below).

## Seeding manifest

One format, three producers. A **manifest** is a list of template
invocations (template name + params, optionally marked deferred-until-
confirmed vs. run-immediately):

```json
{"tasks": [
  {"template": "clone-git-repo", "params": {"repo": "gravitational/teleport", "dest": "~/projects/core"}},
  {"template": "checkout-pr-worktree", "params": {"base": "~/projects/core", "pr": 1234, "dest": "~/projects/teleport-1234"}}
]}
```

- `beams new --clone <pr>` (client CLI sugar) builds one of these and
  either passes it inline to a freshly spawned beam (so the daemon has
  it before its first startup pass) or posts it to an already-running
  daemon's socket for an existing beam.
- The TUI's parameter-entry forms build the same shape before calling
  `run_task`.
- The eventual Mac app ("list of PRs to review → launch in beam") sends
  the same shape — this is the part of the original ask ("should be
  capable of encoding that level of init task") this format is meant to
  satisfy without GUI-specific plumbing on the daemon side.

This is deliberately the *only* interchange format; CLI flags and TUI
forms are sugar on top of it, not parallel paths.

## UX

- `beam-bootstrap tui` connects to the daemon and renders in a zellij
  pane opened to the right (`zellij action new-pane --direction right`),
  not baked into the session's default layout — see "zellij layout" below
  for why that distinction matters.
  - **Top half**: available deferred/templated tasks, with inline param
    input for the ones that take arguments (ratatui + something like
    `tui-input` for the text fields).
  - **Bottom half**: a progress bar plus scrollable log for whatever's
    currently running — fed by the same broadcast task-status events any
    CLI-triggered task also produces.
- `beams task run <template> [params...]` (or similar — exact CLI surface
  TBD) is the agent-friendly on-demand entry point: no TUI required, just
  a command that round-trips through the same socket and prints
  status/completion, suitable for scripting or for an agent to call
  directly.
- Many on-demand tasks are expected to be thin wrappers whose main job is
  reporting rich progress to the bus (e.g. a `git clone` progress-wrapper
  task) rather than doing novel work — the value is the shared progress
  channel, not the task logic itself.

## zellij layout fix

Today's `shell/zellij/beam-shell.kdl` layout's tab template applies to
*every* new tab in the session, which is why opening a second tab
re-shows the bootstrap-log-tail pane (and the ssh-agent-unlock pane,
which just exits immediately since the key's already unlocked by then).
Fix: give the layout an explicit template for the **first** tab only, and
let tabs opened afterward fall back to zellij's plain builtin default
rather than inheriting a session-wide default tab template. The
bootstrap-tui pane (see UX above) is spawned imperatively into the
already-open session instead of being baked into the layout file at all,
which sidesteps the same problem for it from the start.

This is independent of the daemon work and can land on its own.

## Repo delivery onto a beam

Current state: `client/src/stage.rs` computes `git ls-files` in the local
checkout and `scp`s exactly those files — no `.git`, so a beam's copy
isn't a real git repo (hence this session running out of a `beams.git`
dir rather than `beams`).

`tsh git ...` is not viable here — it's scoped to GitHub-org access, not
configured for beam-to-beam or client-to-beam transport, and beams aren't
a GitHub remote.

Chosen approach: **push over the ssh transport the client already uses to
reach the beam**, not a new one.

- Client→beam `tsh ssh` authenticates with the client's own Teleport
  session cert — unrelated to the repo's dedicated keypair in `.ssh/`
  (that pair is for the *beam* to reach GitHub later, e.g. to clone
  `teleport`/`teleport-harness`, not for the client to reach the beam).
  So git can reuse that same authenticated channel as its transport
  (`GIT_SSH_COMMAND` wrapping `tsh ssh`, or whatever `tsh config`-emitted
  ssh_config entries make a plain `ssh <beam-host>` work directly) with
  no new auth problem.
- On the beam: initialize `~/beams` as a normal (non-bare) repo with
  `receive.denyCurrentBranch=updateInstead`, so a push updates the
  working tree in place — no separate checkout/pull step needed.
- Client-side: add it as a git remote, `push` whenever staging/updating a
  beam. Replaces the `git ls-files` + content-copy step. Re-syncing after
  local edits becomes a normal incremental push instead of a full
  re-copy, and the beam ends up with real git history of whatever's been
  pushed (not a flat snapshot) — directly serves "iterate on the tooling
  easily" from a beam itself.
- Still `scp` the two things that were never in git: the repo's own ssh
  keypair, and the prebuilt `beam-bootstrap` binary (no Rust toolchain on
  a stock beam either way).
- Open question to verify before relying on this as the *first* thing
  that happens on a fresh beam: is `git` preinstalled on a stock beam, or
  does it need to ride in via the initial `scp` the same way the binary
  does? Until confirmed, keep the existing scp-based delivery working as
  a fallback rather than cutting over outright.

## New/changed tasks enabled by this

- `clone-git-repo(repo, dest)` — templated, deferred by default.
- `checkout-pr-worktree(base, pr, dest)` — templated, deferred, requires
  `repo-cloned:<base>`.
- `install-toolchain-go`, `install-toolchain-rust` — templated with no
  params (or maybe a version param), deferred by default (expensive,
  not everyone needs both).
- `starship`/fish integration bug — not a design question, just a bug;
  tracked in `TODO.md`, fix independently of the above.

## Explicitly deferred (future, not blocked on anything above, but needs
its own design pass later)

- **Web editor hosting.** `code-server`/`openvscode-server` are the most
  VS-Code-compatible options but heavy (full Node extension host); a
  terminal-based alternative (`ttyd` + neovim/helix) would be far lighter
  if full VS Code UI compat isn't the actual requirement. Worth a
  dedicated research pass against actual beam resource limits when this
  becomes active work, not decided here.
- **Client↔beam folder sharing** (e.g. for `~/projects/core`) via
  mutagen, the same tool already used for `~/rdocker`. Mutagen syncs over
  any ssh command, so it likely composes with the same transport as the
  repo-delivery push above — but needs its own investigation once this
  foundation exists.
- **`russh`-based data channel / on-the-fly port-forward management.**
  Feasible, but Teleport's ssh auth is cert-based via `tsh`'s local
  profile — a russh client would need to load that short-lived cert/key
  and speak to the proxy directly rather than a plain sshd. Start by
  shelling out to `tsh proxy ssh`/`tsh ssh -L` for forwarding; only reach
  for a bespoke russh integration if subprocess-per-forward actually
  becomes a limitation.

## Migration plan

1. `bootstrap`: introduce the task-graph types (capability, template,
   instance, dependency resolution) and the daemon/socket skeleton,
   without changing behavior — migrate every existing step into a
   zero-param template that provides the same capability its current
   idempotency check already detects. `beam-bootstrap --repo ...` keeps
   working by talking to a daemon it spawns if none is running.
2. Land the zellij per-tab layout fix (independent, small).
3. Add the new templated tasks (`clone-git-repo`,
   `checkout-pr-worktree`, toolchains) as deferred templates once the
   graph exists.
4. Build `beam-bootstrap tui` against the daemon.
5. Wire manifest seeding into `client`'s `beams new`/`beams ssh`.
6. Switch repo delivery to the git-push transport once git-on-stock-beam
   is confirmed, keeping scp as fallback until then.
7. Everything in "explicitly deferred" above, each with its own design
   pass when it becomes active work.
