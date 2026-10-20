# beam-bootstrap

Bootstraps a Teleport Beam from a checkout of this `beams` repo: links
dotfiles/`bin` into `$HOME`, installs some apt packages and tools, sets up
fish/starship, and updates `claude`. Safe to re-run — every step checks
what's already in place and skips it.

No client wrapper exists yet (see `project.md`), so for now this is run
manually inside a beam you've already `ssh`'d/`tsh ssh`'d into.

## What it does

1. **link-files** — symlinks `bin` and `.ssh` from the repo checkout into
   `$HOME`. (`.claude` is *not* symlinked — `~/.claude` already holds real
   beam state like session history and credentials.)
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
9. **claude-update** — runs `claude update`.

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
