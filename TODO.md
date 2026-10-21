# TODO

Live task list. `project.md` is the original design brief; this tracks
what's left of it (and anything else found along the way) against what's
actually implemented. Check off / remove items as they land, and add new
ones as they come up — see `AGENTS.md` for conventions. Keep this updated
as part of the same commit as the work it tracks, since this is an
ephemeral beam and nothing else survives.

## Done so far (for context, not actionable)

- `beam-bootstrap`: dotfile linking, apt packages, fish/starship,
  kubectl/zellij install, k3s-bot join + `tbot` as a background service,
  kubeconfig symlink, `claude update`. Idempotent, JSON or human output.
- `beams` client: spawn-or-reuse by purpose, concurrency cap, staging +
  scp of repo + prebuilt bootstrap binary, `tsh ssh` into `beam-shell`.
- `bin/beam-shell`: zellij layout with main pane, bootstrap log tail, and
  an async ssh-agent-unlock pane that never blocks a noninteractive run.

## Open tasks

### Bootstrap wizard / optional expensive steps
- [ ] project.md calls for "a small wizard to pick expensive things to
  enable: whether or not to clone Teleport, which PR to check out (if
  any; should be scriptable)". Not started. Needs:
  - A way for `beam-bootstrap` to prompt (or accept flags/env for
    non-interactive/scripted use) before running expensive steps.
  - A `clone-teleport` step (with PR checkout support) — doesn't exist
    yet in `bootstrap/src/steps/`.
  - Decide how this interacts with the existing fixed step order / the
    `--only` flag.

### Bidirectional progress reporting
- [ ] project.md wants progress reported back to the *caller* over ssh,
  bidirectionally (not just the local zellij log pane) — e.g. for the
  `beams` client itself to render progress during the initial `tsh ssh`
  call rather than only after dropping into zellij. Current state: the
  client's first connection just execs into `beam-shell`, which is
  zellij-only; there's no protocol for the client to consume bootstrap
  events directly.
- [ ] Longer-running async sub-tasks (teleport clone, go toolchain
  install, etc. — once they exist) should get actual progress bars in
  the zellij log pane, per project.md. Right now the log pane just tails
  plain step events; no sub-step progress bars exist because none of the
  steps that would need them (clone, toolchain install) exist yet either.

### Git / file sync between client and beam
- [ ] project.md flags this as open for discussion: beyond the one-shot
  `tsh beams scp` staging copy, there's no ongoing sync of this repo (or
  others) between workstation and beam. Options noted: plain git
  push/pull with a nicer UX on top, or a mutagen-style live mirror (already
  used for `~/rdocker`). Needs a decision, not just an implementation.
- [ ] Related: since `beams.git`'s ssh private key can't go through a
  normal `git clone`, any sync design has to account for that (today it's
  solved by shipping the key via `scp` as part of staging — fine for this
  repo, but doesn't generalize to syncing *other* repos onto a beam).

### k8s access workaround
- [ ] `k3s-bot-join` / `tbot` kube output exists purely because "beams k8s
  access doesn't work currently" per project.md. Periodically check
  whether native beams k8s access has been fixed upstream; if so, this
  whole workaround (bot-onboarding, `k3s_bot.rs`, `tbot_service.rs`,
  `kubeconfig.rs`) could potentially be dropped in favor of direct access.

### Testing
- [ ] `bootstrap` steps have no automated tests (see `AGENTS.md`). At
  minimum, consider extracting the pure logic (e.g. version/checksum
  parsing in `releaseutil.rs`, bot-join token/already-joined checks in
  `k3s_bot.rs`) into unit-testable pieces, separate from the
  side-effecting parts.
- [ ] No CI configured (`.github/workflows` doesn't exist). At minimum a
  `cargo check`/`cargo test` workflow for both `bootstrap/` and `client/`
  would catch breakage before it's discovered on a live beam.

### Misc / smaller
- [ ] `tsh beams publish` (HTTP/TCP app publishing from a beam) isn't
  integrated into either binary yet — not clear if/how it fits the
  client's UX (would it be another `beams` subcommand?).
- [ ] No cleanup/expiry helper on the client side beyond the 5-beam cap
  check at spawn time — e.g. a `beams ls`/`beams rm` convenience wrapper
  around `tsh beams ls/rm`, or a warning when a tracked purpose's beam is
  close to its TTL even if not actively reused.
- [ ] Confirm whether `teleport-harness` (the actual consumer of the k3s
  workaround, per project.md) has been exercised end-to-end from a beam
  yet, or is still just a theoretical target.
