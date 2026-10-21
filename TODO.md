# TODO

Live task list. See `DESIGN.md` for the current architecture direction
(daemon + capability-based task graph) and `project.md` for the original
brief. Check off / remove items as they land, add new ones as they come
up. Per `AGENTS.md`'s session workflow: update this in the *same commit*
as the work it tracks — this file, not conversation history, is what the
next session trusts.

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

- [ ] Introduce task-graph types in `bootstrap`: capability, template,
  instance, dependency resolution by capability (not task ID) — see
  DESIGN.md's "Task model". No behavior change yet: migrate every
  existing step into a zero-param template providing the capability its
  current idempotency check already detects.
- [ ] Daemon skeleton: `beam-bootstrap daemon`, UDS socket at
  `~/.beam-bootstrap/daemon.sock`, newline-delimited JSON protocol with
  `request_id` correlation, task-status events broadcast to all
  connected clients. `beamctl start beam-bootstrapd -- beam-bootstrap
  daemon` to launch it, same mechanism as `tbot` today.
- [ ] `beam-bootstrap --repo ...` (today's one-shot entrypoint) becomes:
  ensure a daemon is running, then let the daemon run the startup tasks,
  rather than running them itself inline.
- [ ] Manifest format (see DESIGN.md's "Seeding manifest") — the one
  interchange shape for template invocations, used by CLI sugar, TUI
  forms, and (eventually) a Mac app alike. Implement the manifest schema
  + a `run_task`/`list_tasks` socket op before building any UI on top.
- [ ] `beam-bootstrap tui`: ratatui client, connects to the daemon's
  socket, deferred/templated task list + param input on top, progress
  bar + scrollable log on bottom.
- [ ] `beams task run <template> [params...]` — agent-friendly on-demand
  CLI entry point, no TUI required.
- [ ] Wire manifest seeding into `client`'s `beams ssh`/`beams new` (e.g.
  `beams new --clone <pr>` builds a manifest and hands it to a freshly
  spawned beam before its first startup pass).

## New tasks enabled by the graph (deferred-by-default templates)

- [ ] `clone-git-repo(repo, dest)`.
- [ ] `checkout-pr-worktree(base, pr, dest)` — requires
  `repo-cloned:<base>`.
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
