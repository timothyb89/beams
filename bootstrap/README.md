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
it in place after installing Rust yourself:

```sh
# one-time, if this beam doesn't have Rust yet
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
sudo apt-get install -y gcc   # cargo needs a linker
source "$HOME/.cargo/env"

cd bootstrap
cargo build            # debug build: target/debug/beam-bootstrap
cargo build --release  # release build: target/release/beam-bootstrap
```

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
ssh-agent is running and, since this runs on a plain terminal before zellij
takes over, opportunistically prompts to unlock the repo's key if it isn't
loaded yet (see "ssh-agent" below), runs `beam-bootstrap` in the background
logging to `~/.beam-bootstrap.log`, then attaches to (or creates) a `beam`
zellij session with a layout that keeps that log visible in a side pane
(`shell/zellij/beam-shell.kdl`):

```sh
~/bin/beam-shell
```

Safe to run repeatedly - it's just the attach-or-create flow each time, and
`beam-bootstrap` is idempotent.

### ssh-agent: interactive unlock, noninteractive-safe

`bin/ssh-agent-init` (and its fish port, `.fish`) always finds or starts the
agent first - safe to source from anywhere, interactive or not. It only
attempts to unlock and `ssh-add` the key (which may prompt for a
passphrase) when stdin/stdout are both a real tty; otherwise it leaves the
key as-is rather than blocking on a prompt nobody can answer. `beam-shell`
sources it before zellij takes over for exactly that reason. `config.fish`
also sources it unconditionally on every fish start - since fish is the
beam's default login shell, that covers both an interactive login *and* a
noninteractive `tsh ssh ... -- cmd` exec, so commands run that way work
against whatever the agent already holds without ever hitting a password
prompt.
