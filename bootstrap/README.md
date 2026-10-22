# beam-bootstrap

Bootstraps a Teleport Beam from a checkout of this `beams` repo: links
dotfiles/`bin` into `$HOME`, installs some apt packages and tools, sets up
fish/starship, and updates `claude`. Safe to re-run — every step checks
what's already in place and skips it.

`bin/beam-shell` runs this automatically after copying the repo onto a
fresh beam (see its own section below), or the client wrapper in
`../client/` runs the whole thing for you via `beams ssh`. You can also
still run it manually inside a beam you've already `ssh`'d into.

## What it does

The steps below run in dependency order, resolved from each step's
declared capabilities (`provides`/`requires` in `src/steps/mod.rs`, see
`DESIGN.md`) rather than a hardcoded sequence, and **concurrently**
wherever the graph allows it (see `DESIGN.md`'s "Concurrent execution") -
e.g. `apt-install` and `install-teleport-bin` run at the same time, not
one after the other just because they're both in this list. The numbered
order below is just this doc's presentation order (and still a valid
*serial* order, dependency-wise) - the steps that shell out to
`apt-get`/`dpkg` (`apt-install`, `install-kubectl`) are tagged so they
never run concurrently with *each other*, even though nothing else stops
them from doing so.

1. **link-files** — symlinks `bin` and `.ssh` from the repo checkout into
   `$HOME`. `.ssh` is force-replaced: a fresh beam ships its own default
   `~/.ssh` (a delegated signing agent, not a plain key), which this moves
   aside to `~/.ssh.orig` the first time, so the repo's stable key always
   wins. (`.claude` is *not* symlinked — `~/.claude` already holds real
   beam state like session history and credentials, so it can't be
   replaced wholesale the same way.)
2. **claude-md-merge** — appends the repo's `.claude/CLAUDE.md` into
   `~/.claude/CLAUDE.md` once.
3. **apt-install** — installs `micro`, `less`, `fish`.
4. **install-kubectl** — adds the official Kubernetes apt repo and installs
   `kubectl` (not in Debian's default repos).
5. **install-zellij** — downloads a pinned `zellij` release binary from
   GitHub and verifies its checksum (not packaged for Debian at all).
6. **shell-config** — symlinks `shell/fish/config.fish` and
   `shell/starship.toml` from the repo into `~/.config/`.
7. **install-starship** — downloads a pinned `starship` release binary from
   GitHub and verifies its checksum (not packaged for Debian either).
8. **default-shell** — sets fish as the user's login shell (`chsh`).
9. **install-teleport-bin** — downloads `tctl`/`tbot` from
   `cdn.teleport.dev`, matching the cluster's proxy version
   (`tsh version -f json`), with sha256 verification, into
   `~/.teleport-bot/bin/`.
10. **k3s-bot-join** — requests a join token for the `k3s-bot` bot (see
    `bot-onboarding/`, applied once, cluster-wide, separately from this
    tool) and writes a static `~/.teleport-bot/tbot.yaml` with a Kubernetes
    output for the `k3s` cluster and an identity output for SSH to its host
    node. Skips requesting a new token once already joined
    (`~/.teleport-bot/storage/key` exists).
11. **start-tbot** — runs tbot as a `beamctl`-managed background service
    (`beamctl show tbot`) so both outputs keep renewing for the beam's
    lifetime.
12. **kubeconfig-symlink** — symlinks tbot's generated
    `~/.teleport-bot/kube/kubeconfig.yaml` into `~/.kube/config`, so
    `kubectl` works against the k3s cluster with no extra flags.
13. **claude-update** — runs `claude update`.
14. **start-bootstrap-daemon** — starts `beam-bootstrap daemon` (see
    "Daemon" below) as a `beamctl`-managed background service, same
    pattern as `start-tbot`. No dependencies on anything else here, so
    it comes up as early as possible - `bin/beam-shell` also runs this
    one synchronously (like `link-files`/`install-zellij`/etc.) before
    launching zellij, since the `tui` pane in its layout needs something
    to connect to right away.

SSH to the k3s host node isn't wired into `tsh`/`ssh` directly yet - use the
identity output explicitly for now:

```sh
tsh --identity=~/.teleport-bot/identity/identity ssh core@workstation
```

If a step fails, the ones after it don't run; fix the issue and re-run the
whole binary — earlier steps will just skip.

## Building

No Rust toolchain ships on a stock beam, so build this on a machine that has
one (this sandbox, or your workstation) and copy the binary over, or build
it in place after installing Rust yourself.

### From a Linux machine (e.g. this sandbox)

```sh
# one-time, if this beam doesn't have Rust yet
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
sudo apt-get install -y gcc   # cargo needs a linker
source "$HOME/.cargo/env"

cd bootstrap
cargo build            # debug build: target/debug/beam-bootstrap
cargo build --release  # release build: target/release/beam-bootstrap
```

### Cross-compiling from macOS (or anywhere else)

Beams run Linux, so building directly only works when you're already on
Linux. From macOS - or to get a statically linked build with no glibc
version to worry about, even from Linux - cross-compile instead:

```sh
# one-time setup
rustup target add x86_64-unknown-linux-musl
cargo install cargo-zigbuild
pip3 install ziglang   # or `brew install zig` - cargo-zigbuild just needs
                        # a `zig` toolchain available one way or the other

./bootstrap/build-cross.sh
```

Produces `bootstrap/target/x86_64-unknown-linux-musl/release/beam-bootstrap`
- a statically linked ELF binary, runnable on any Linux beam regardless of
its glibc version. `client/src/stage.rs` looks for a build at this path
first when assembling what to copy onto a beam, falling back to a native
`target/release`/`target/debug` build (and refusing to ship anything that
isn't actually a Linux ELF binary - e.g. a stray native macOS build sitting
at the fallback path).

## Running

```sh
./target/debug/beam-bootstrap --repo /home/beams/beams
```

- `--repo <path>` — path to the beams repo checkout to bootstrap from.
  Defaults to `~/beams`.
- `--json` — force JSON-lines event output even on a TTY. Output is JSON
  automatically when stdout isn't a TTY (e.g. piped), otherwise it's
  rendered human-readable. Each line looks like:

  ```json
  {"type":"step","step":"apt-install","status":"ok","message":"installed micro, less"}
  ```

  `status` is one of `start`, `ok`, `skip`, `error`.
- `--only <step-id>` — run just that one step (e.g. `link-files`) instead
  of the full sequence. `bin/beam-shell` uses this to get `~/.ssh`/`~/bin`
  in place synchronously before anything that needs them, ahead of
  backgrounding the full run.

Exit code is `0` if every step succeeded (or was skipped), `1` if any step
failed.

### On-demand templated tasks

Separate from the fixed startup sequence above: `src/tasks/` holds
templated tasks that take runtime params instead of running
unconditionally at startup (see `DESIGN.md`'s "Templates vs. instances").
Two ways to reach them - directly (no daemon involved), or through the
daemon (see "Daemon" below):

```sh
./target/debug/beam-bootstrap list-tasks
./target/debug/beam-bootstrap run-task clone-git-repo --repo <url-or-path> --dest ~/projects/core
./target/debug/beam-bootstrap run-task checkout-pr-worktree --base ~/projects/core --pr 1234 --dest ~/projects/teleport-1234
```

Each runs exactly one task in-process - no dependency resolution against
other tasks (so run `clone-git-repo` against `--base` before
`checkout-pr-worktree` depends on it having happened). Idempotent like
the startup steps: re-running `clone-git-repo` against an existing
checkout, or `checkout-pr-worktree` against an existing worktree, is a
no-op.

### Daemon

`beam-bootstrap daemon` listens on a UDS socket (`--socket`, defaults to
`~/.beam-bootstrap/daemon.sock`) for `list_tasks`/`run_task`/
`run_manifest` requests as newline-delimited JSON - see `src/daemon.rs`'s
module doc comment for the full protocol. Meant to be supervised, e.g.:

```sh
beamctl start --name beam-bootstrapd -- ./target/debug/beam-bootstrap daemon
```

Nothing drives this from the fixed startup sequence, a TUI, or
`client/` yet (see `DESIGN.md`'s migration plan) - today it only exists
so these ops are reachable over a socket at all, same instantiation path
as `run-task` above (`tasks::instantiate`), with task-status progress
broadcast to every connected client, not just whoever asked - try it
with two terminals:

```sh
# terminal 1
./target/debug/beam-bootstrap daemon

# terminal 2 - or any other UDS client, e.g. `socat - UNIX-CONNECT:...`
echo '{"request_id":"1","op":"run_task","task":"clone-git-repo","params":{"repo":"...","dest":"..."}}' \
  | nc -U ~/.beam-bootstrap/daemon.sock
```

`run_manifest` is `run_task` generalized to several task instances at
once, resolved against each other (so they can be listed in any order)
via the same capability graph the fixed startup sequence uses - this is
what lets `checkout-pr-worktree` and the `clone-git-repo` it depends on
be requested together in one call:

```sh
echo '{"request_id":"2","op":"run_manifest","tasks":[
  {"template":"checkout-pr-worktree","params":{"base":"~/projects/core","pr":1234,"dest":"~/projects/teleport-1234"}},
  {"template":"clone-git-repo","params":{"repo":"...","dest":"~/projects/core"}}
]}' | nc -U ~/.beam-bootstrap/daemon.sock
```

## TUI

`beam-bootstrap tui [--socket ...]` is a ratatui client for the daemon
(requires one already running - it doesn't spawn its own). Top half:
available templates (↑/↓ to pick, enter to open a param-entry form, tab
to move between fields, enter on the last field to submit, esc to
cancel). Bottom half: one row per task the daemon's broadcast anywhere
mentioned - not just tasks submitted from this TUI instance - tagged
with its wave (`[wN]`) so concurrently-running tasks are visibly grouped,
plus an aggregate gauge (`done/total, N running`) above them. `r`
refreshes the template list, `tab` (outside a form) switches focus
between the template list and the task list for scrolling, the scroll
wheel moves whichever of those is focused the same way the arrow keys
do, `q`/`esc` quits.

If the daemon connection drops (e.g. `beamctl restart beam-bootstrapd`
mid-session), the status line says so and the TUI retries every second
until it's back - rows from before the drop are left as-is (whatever
happened to those tasks while disconnected is simply unknown), but the
template list is re-fetched the moment reconnection succeeds.

Only submits one task at a time today (`run_task`), even though the
daemon's `run_manifest` can resolve and run several together - there's
no multi-select-and-submit-as-a-manifest UI yet (see `TODO.md`). Every
other client's `run_manifest` activity still shows up live, since
`task_status` broadcasts go to every connection regardless of who
triggered them.

## Trying it out

```sh
cd bootstrap
cargo build
./target/debug/beam-bootstrap --repo ~/beams
# run it again - everything should now report "skip" except claude-update
./target/debug/beam-bootstrap --repo ~/beams
```

## bin/beam-shell

A convenience entrypoint for day-to-day use, once `cargo build --release`
has produced the binary (or `cargo build` for a debug one): makes sure the
ssh-agent is running (never blocks - see "ssh-agent" below), runs
`beam-bootstrap` in the background logging to `~/.beam-bootstrap.log`,
then attaches to (or creates) a `beam` zellij session
(`shell/zellij/beam-shell.kdl`) with three panes: the main one, a log pane
tailing `~/.beam-bootstrap.log`, and an `ssh-agent` pane that handles the
actual interactive key unlock whenever you get to it:

```sh
~/bin/beam-shell
```

Safe to run repeatedly - it's just the attach-or-create flow each time, and
`beam-bootstrap` is idempotent.

### ssh-agent: async unlock, noninteractive-safe

`bin/ssh-agent-init` (and its fish port, `.fish`) always finds or starts the
agent first - safe to source from anywhere, interactive or not. It only
attempts to unlock and `ssh-add` the key (which may prompt for a
passphrase) when stdin/stdout are both a real tty; otherwise it leaves the
key as-is rather than blocking on a prompt nobody can answer.

`beam-shell` sources it once, with stdin redirected from `/dev/null` so
the interactivity check reads false even though beam-shell itself is
running on a real terminal - deliberately forcing the noninteractive
("ensure only, never unlock") path. That's sourced (not exec'd), so the
agent's env ends up exported in beam-shell's own process - which `exec
zellij` then hands down to every pane zellij spawns. The zellij layout's
dedicated `ssh-agent` pane runs `ssh-agent-init` again on its own, this
time with no redirect: the agent's already up (inherited from beam-shell,
so this is instant), and this pane *does* have a real tty, so the unlock
prompt runs there - whenever you switch over to it. Nothing else needs to
wait on that; the agent itself is already reachable to every pane from the
moment zellij starts, unlocked or not.

`config.fish` also sources it unconditionally on every fish start - since
fish is the beam's default login shell, that covers both an interactive
login *and* a noninteractive `tsh ssh ... -- cmd` exec, so commands run
that way work against whatever the agent already holds without ever
hitting a password prompt.
