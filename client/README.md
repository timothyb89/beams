# beams (client wrapper)

Runs on your local workstation (not inside a beam). Spawns or reuses a
Teleport Beam and drops you straight into it, per project.md's "secondarily"
section. Shells out directly to `tsh beams ls/add/scp` and `tsh ssh`, always
using your current `tsh` identity - no admin credentials, no dependency on
`~/teleport-client-rs` (see commit history for why: its reusable surface,
`TshExecutor`, only covers 2 of the ~5 operations needed here, and would
pull in a heavy gRPC/TLS dependency tree for that).

## `beams config validate` / `explain`

The offline launch-profile compiler is available for inspecting the new
configuration contract:

```sh
beams config validate                     # bundled development starter
beams config explain --profile builtin:core
beams config validate --profile /path/to/environment.toml
beams config explain --profile /path/to/environment.toml --json
```

Profiles use strict TOML (`schema_version = 1`), local/bundled inheritance,
named files and script/template tasks, and pre/post package lists.
Validation checks sources, parameters, package conflicts, destination
conflicts, and graph ordering. Explain shows origins, batches, selected
artifact roots and dependencies. It checks declared root metadata without
recursively scanning directories. Both commands work without Teleport, a
Beam, or a source checkout; built-in profiles/assets are embedded.

`--json` includes workstation source metadata alongside a remote-facing
`plan`; it is inspection output, not a daemon launch request yet. These
commands do not install, upload, or execute anything. `beams ssh` still uses
its existing startup path. Profile selection during SSH, archive upload,
and `config init` are pending. An opt-in foreground execution command is
available in bootstrap; daemon acceptance/readiness wiring remains pending.

See the [developer guide](../docs/development/launches.md) for the supported
schema, ordering rules, custom templates and a checked team example in
`environments/examples/team/`. Copy the example to `environments/local/`
(gitignored) to customize it.

## `beams config pack` / `pack-post`

```sh
beams config pack --profile /path/to/environment.toml --output /tmp/launch
beams config pack-post --launch-dir /tmp/launch
```

Pack creates a new private launch directory with `launch.json` (UUID and
plan digest), workstation-only `local-inputs.json`, and `pre/descriptor.json`
plus `pre/bundle.tar.gz`. It freezes only pre sources. `pack-post` uses the
saved declarations to snapshot post sources later; repeating it reuses
the frozen spool, even if the original sources have changed.
`--compression none` produces plain `bundle.tar`; gzip uses level 1.
Existing launch output is refused rather than overwritten.

Tar entries contain `bundle.json` and staging-relative payload paths, with
content hashes, ordinary modes, directories and validated internal symlinks.
Traversal, escaping/dangling links and special files fail. The remote
`beam-bootstrap launch unpack` receiver verifies and publishes staging
without installing targets. Upload only `launch.json` and the phase
archive/descriptor, keeping `local-inputs.json` on the workstation.
These are profile payload bundles; runtime/Zellij delivery is still pending.
`beams config launch` (below) does the upload, submission and post packing
for you.

After unpacking to `STAGING/pre` and `STAGING/post`, bootstrap can execute
the graph with `launch run-phase --launch launch.json --staging STAGING
--phase pre`, then `--phase post --headless`. This installs selected targets
and runs hooks through the existing executor. It requires a successful
pre receipt for post and does not connect to the daemon or spawn Zellij.
See [bootstrap's README](../bootstrap/README.md#foreground-launch-execution)
and the developer guide for ordering and retry behavior.

## `beams config launch`

```sh
beams config launch --profile /path/to/environment.toml [TARGET] [--headless] [--no-follow]
```

Packs the profile (as `config pack`, into
`~/.config/beams-client/launches/<uuid>/`) and hands it to the local daemon,
which drives it onto an existing Beam (`TARGET`: id, uuid or purpose;
default the `default` purpose) over that Beam's bridge, so the Beam needs
this build's `beam-bootstrap` running. No scp or extra SSH connection: the
daemon uploads the declaration, submits it, then uploads the pre bundle in
chunks over the already-open bridge; the Beam verifies and stages each
bundle itself and starts the work that needs it. Once the Beam reports
`interface-ready` the daemon packs post (so a large post tree never delays
pre) and uploads it. The command prints status and upload progress until
the launch finishes and exits 0/1 (`--no-follow` returns once the daemon
has it).

`--headless` releases post work as soon as pre succeeds. Without it, post
waits for the Beam's initial shell and TUI to report ready
(`BEAMS_LAUNCH_ID`), which `beams ssh` does not set up yet; until then use
`--headless`, or `beam-bootstrap launch ready`/`launch signal` on the Beam.

The daemon records each launch (Beam, launch directory, gate, last status
and released conditions) in `daemon-state.json` before uploading anything.
Whenever the Beam's bridge reconnects (including after a daemon restart),
it drives unfinished launches again: the Beam answers with what it already
holds, so a resubmission starts nothing new and an interrupted upload
resumes from the bytes the Beam has. Frontends receive `launch_changed`
events and the launches in the `snapshot`; the session overlay doesn't
show them yet. Protocol and flow: the developer guide's
[launches over the workstation bridge](../docs/development/launches.md#launches-over-the-workstation-bridge).
`client/tests/launch-bridge-smoke.sh` runs it end to end against local
daemons with a fake `tsh`.

## `beams ssh`

```sh
beams ssh [--repo ~/beams] [--purpose default] [--region REGION] [--login beams]
```

1. Looks up the beam tracked for `--purpose` (see "State" below). If it
   exists and has more than 12 hours of TTL left (beams can't have their
   TTL extended), reuses it - no file copy, straight to connecting.
2. Otherwise spawns a new beam (`tsh beams add`), refusing if already at
   the 5-concurrent-beam limit (`tsh beams rm` something first).
3. Pushes `--repo`'s current branch onto the beam over git's `ext::`
   transport, run through the same `tsh ssh` channel used everywhere
   else here - no new credential, not the repo's own dedicated ssh
   keypair (see `docs/designs/DESIGN.md`'s "Repo delivery onto a beam" for the full
   design, including a real gotcha found verifying this against a live
   beam: the beam-side repo's checked-out branch has to match what gets
   pushed, or `receive.denyCurrentBranch=updateInstead` silently no-ops
   instead of erroring). Only transfers committed history, like any git
   push. Also points the beam's own `origin` remote at `--repo`'s real
   `origin` (plain GitHub URL, not the `ext::` one used for this push,
   which only works from this workstation) - without this the beam ends
   up with no `origin` configured at all, since neither `git init` nor
   a push over a bare URL creates one. Skipped entirely if `--repo`
   itself has no `origin` configured.
4. Separately `scp`s the required extra deliberately *not* tracked in git:
   a prebuilt `beam-bootstrap` binary (**build this first**: beams run Linux, so if
   you're on macOS (or just want a statically linked build) run
   `../bootstrap/build-cross.sh`; on Linux, `cd ../bootstrap && cargo
   build --release` also works. No Rust toolchain on a fresh beam, so the
   binary has to be copied over either way - see `../bootstrap/README.md`.
   `stage.rs` looks for a cross-compiled build first and falls back to a
   native one, refusing to ship anything that isn't a Linux ELF binary.)
5. Registers a random managed-session token with the local daemon's
   per-Beam bridge, then connects with `tsh ssh -t <login>@<uuid> -- env
   BEAMS_CLIENT_ID=... BEAMS_SESSION_TOKEN=... ~/beams/bin/beam-shell` -
   bypassing the `tsh beams ssh` layer (which can't take a remote command)
   in favor of plain `tsh ssh`, per project.md. Unlike the original direct
   `exec` handoff, `beams` now supervises `tsh` under a local PTY: terminal
   bytes, resize events, exit status, and signals still pass through, while
   `Ctrl-]` can temporarily open a local menu over the remote session.
   `beam-shell` starts the daemon's SSH agent relay, kicks off
   `beam-bootstrap` in the background, and drops you into zellij with a
   TUI and log pane - see `../bootstrap/README.md`.

On reuse, steps 3-4 are skipped entirely - a reused beam is assumed to
already be bootstrapped from a prior run.

## Managed SSH session and local menu

During `beams ssh`, press `Ctrl-]` to open the workstation-side Beams menu.
Press `q`, escape, or enter to close it; press `Ctrl-]` a second time to close
the menu and send `Ctrl-]` to the remote terminal. Configure the
prefix with `--escape-key`:

```sh
beams ssh --escape-key ctrl-g
beams ssh --escape-key 0x1d
beams ssh --escape-key none   # pure passthrough; local menu disabled
beams ssh --terminal-overlay # experimental shadow-screen compositing
```

The input router recognizes both legacy control bytes and the Kitty CSI-u /
xterm modifyOtherKeys sequences that full-screen applications such as zellij
can enable in modern terminals (including iTerm2). Unmatched terminal
sequences are still forwarded byte-for-byte.

When a new remote request for the current Beam arrives, a toast appears in
the bottom-right corner ("Fetch big.bin (60 MB) · Ctrl-] to review") without
taking input: typing keeps going to the remote session. Pressing the menu key
while it's up opens that request directly (its path form, or the
approve/reject menu), and submitting, deciding, or backing out returns
straight to the session. An ignored toast disappears after 20 seconds; the
request stays pending in the menu until the local daemon expires it (60
seconds). Running copies and fetches show a progress bar in the same corner,
then a brief done or failed toast. Requests belonging to other managed Beams
don't produce toasts; the menu never opens by itself. `--escape-key none` retains
strict passthrough and disables toasts along with the manual menu.

Toasts are drawn over live remote output in both display modes, using the
shadow terminal model the session keeps anyway. They are written only between
escape sequences (the session tracks the remote stream's parser state,
including OSC/DCS strings, split UTF-8, and the remote's own synchronized
updates), inside a synchronized update of their own; afterwards the cursor and
drawing attributes are restored from the model rather than with DECSC, whose
single slot the remote may be using. A toast is repainted once remote output
settles (or every 250 ms during a long burst), and the cells it covered are
repainted from the model when it goes away or the session ends. A remote that
enables origin mode or insert mode, or selects the DEC line-drawing
character set, could see a toast drawn incorrectly; the model doesn't expose
those modes.

The menu shows connection/bridge state, then one selectable list: every
pending remote request for the current Beam (oldest first), then that
Beam's local services, each with a colored status. `↑`/`↓` (or `j`/`k`,
`Home`/`End`) move the selection. Selection follows the row itself, so a
request decided elsewhere or a service that disappears doesn't shift it onto
a different row. When a new request opens the menu by itself, that request
is selected.

- `Enter` on a request opens its full description with Approve / Reject /
  Decide later. `y`/`n` decide the selected request directly, or the oldest
  one when a service is selected.
- `Enter` on a service opens its details (full endpoint, status, last
  message, ignore rules, service id) and the actions that make sense in its
  current state: no stop for a stopped forward, resume only when paused.
  The same actions also have shortcuts on the home screen: `s` stop,
  `r` restart, `p` pause, `e` resume, `t` terminate.
- Irreversible actions ask first, with "Keep it" pre-selected, so `Enter`
  alone never destroys anything. That covers terminating a folder sync
  (removes its Mutagen session) and cancelling a file copy (the job can't be
  resumed).
- `f` opens a form for a client-initiated port forward (Beam port, optional
  workstation port). `u`/`d` open an upload/download form (workstation path,
  Beam path, overwrite toggle). `m` opens a folder-sync form (directories,
  mode, VCS/ignore rules).
- Approving a remote file-copy or folder-sync request opens a
  path-confirmation form first. The Beam-provided value is only a hint and
  cannot select a workstation path by itself.

`q` or `Esc` closes the menu. Completed, failed, cancelled, or interrupted
copies are terminal; create a new job instead of restarting one.

Forms behave like ordinary text inputs: `Tab`/`Shift-Tab` (or `↓`/`↑`) move
between fields; `←`/`→`, `Home`/`End`, `Backspace`/`Delete`, and the readline
chords `Ctrl-A`/`E`/`U`/`K`/`W` edit text; pasted text lands in the focused
field; `Space` or `←`/`→` change toggles and choices. Workstation path fields
show a dimmed completion from the local filesystem (`→` at the end of the
text accepts it) and a dropdown of the typed directory's entries,
fuzzy-filtered by the segment being typed (`dl` finds `Downloads`; hidden
entries appear once the segment starts with `.`). In the dropdown, `↓`/`↑`
(or `Ctrl-N`/`Ctrl-P`, `PageDown`/`PageUp`) highlight an entry and `Enter`
takes it; `Tab` completes the common prefix first, then the highlighted or
best entry, and moves to the next field once the path is complete; `Esc`
hides the list until the text changes. A directory entry keeps the list open
on its contents. While a path field is empty the dropdown offers recently
used workstation directories, kept in
`~/.config/beams-client/recent-paths.json`. Relative and `~` workstation paths are resolved against the
directory `beams ssh` was started from. Validation errors appear under the
offending field and the form stays open with its input intact; `Enter`
submits and `Esc` cancels. Menu keys are decoded from plain bytes, `ESC [`
and application-cursor `ESC O` sequences, Kitty CSI-u, and modifyOtherKeys,
so they work whichever keyboard mode the remote application left the outer
terminal in.

The client does not nest another terminal alternate screen over remote zellij
(alternate-screen state is not stackable). It drains up to 1 MiB of remote
output while the menu is visible, restores that output on close, and briefly
resizes/restores the child PTY with distinct notifications to make the remote
application redraw. Every menu frame clears stale cells before painting, and
the dialog is drawn on a cleared panel, so local resizes and shorter updates
do not retain content from an older frame. Terminal raw mode and cursor
visibility are restored on normal exit, errors, and handled termination
signals. Daemon request, bridge, and service events repaint an open menu
immediately; closing and reopening it is not required to refresh status.

`--terminal-overlay` is an experimental alternative for testing local
compositing. The client continuously feeds remote output into a `vt100`
shadow screen while still passing the original bytes through unchanged. When
the local menu is open, that captured screen is painted, dimmed, underneath
the same dialog the takeover mode draws (both modes share one renderer in
`src/session/ui.rs`); remote output continues updating the shadow screen
while it is hidden from the physical terminal. A persistent ratatui renderer
updates only changed cells, avoiding the full-screen clear/redraw flicker of
the first prototype. Closing an overlay restores the model without sending a
synthetic one-column resize pulse to the remote PTY. OSC title/clipboard
requests, mouse mode, bracketed paste, alternate-screen state, resize, colors,
and wide characters are tracked by the model, but clipboard actions and
unsupported terminal features are not interpreted by the model yet. Complete
OSC sequences are still passed through to the physical terminal, preserving
the existing title/clipboard behavior. The existing takeover path remains the
fallback and default.

The controlling-terminal smoke test exercises both paths:

```sh
./client/tests/managed-session-smoke.sh
BEAMS_TEST_OVERLAY=1 ./client/tests/managed-session-smoke.sh
```

## Local service daemon

`beams ssh` connects to a per-user workstation daemon and starts one in the
background if no instance is listening. The foreground SSH process still owns
the interactive PTY. The daemon owns one reconnecting
`tsh ssh ... ~/beams/bin/beam-client-bridge` process per Beam, shares it across
local frontends, retains all managed-shell tokens when that bridge reconnects,
and supervises approved `tsh ssh -N -L` processes plus one-shot `tsh scp`
jobs independently of the interactive terminal.

A bridge only resets its retry backoff (250ms doubling to 5s) after the Beam
actually answers its hello. Once a bridge has been failing for 20 seconds, the
daemon checks `tsh beams ls` at most once a minute. If the Beam is gone, it
retires the bridge, stops that Beam's forwards and copies, and pauses its folder
syncs. If `tsh` can't list Beams (offline, logged out), the bridge keeps
retrying. A later `beams ssh` to the same Beam starts a fresh bridge. The
local menu only shows bridge status for its own session's Beam.

Run it explicitly in the foreground for debugging:

```sh
beams daemon
beams daemon --socket /tmp/beams-test.sock
```

`--no-client-daemon` on `beams ssh` disables connection/auto-start without
disabling the managed PTY itself.

### Daemon lifecycle and updates

```sh
beams daemon status            # PID, version, executable, busy/idle
beams daemon stop [--force]    # graceful shutdown
beams daemon restart [--force] # stop, then start one from this binary
```

The daemon updates itself. Every 15 seconds it checks whether the executable
it started from has been replaced (by a rebuild or reinstall). Once the new
file has been stable for a full check and nothing would be lost, it re-execs
the new binary in place. "Nothing would be lost" means no file copy in
flight and no approval pending. The PID stays the same, the listening socket
is handed over (it never disappears, so no frontend can race in a second
daemon), and the forwards that were running are restored. Folder syncs are
re-adopted. Open `beams ssh` sessions reconnect and re-register their
bridges on their own.

`stop` and `restart` refuse while the daemon is busy unless `--force` is
given. They use two control messages, `status` and `shutdown`, whose shape
never changes and which work without a protocol handshake, so any `beams`
can manage any daemon that understands them. A daemon too old for those
messages is sent SIGTERM via the PID the kernel reports for the socket's
peer. If `beams ssh` meets a daemon speaking a different protocol, it
replaces it the same way. It does so only when its own binary hasn't been
replaced since it started, so a stale session can't take down a newer
daemon.

The default socket is `$XDG_RUNTIME_DIR/beams-client/daemon.sock` when
`XDG_RUNTIME_DIR` is set, otherwise
`~/.config/beams-client/daemon.sock`. It and the versioned state file
`~/.config/beams-client/daemon-state.json` are mode `0600`; the state file is
replaced atomically and contains the workstation's stable random `client_id`
plus the request/service registry. Service records contain typed endpoints,
desired state, and bounded diagnostic summaries, but never persisted PIDs.
Auto-start diagnostics go to
`~/.config/beams-client/daemon.log`.

The local protocol is typed, versioned newline-delimited JSON. It requires a
frontend handshake, limits frames to 64 KiB, bounds event queues, rate-limits
each connection, and times out incomplete handshakes. Frontends receive an
initial snapshot and subsequent daemon, bridge, and request broadcasts. If the
local daemon restarts while an interactive session remains open, that frontend
reconnects, re-registers its Beam/token pair, and keeps the remote shell
running. Pending approvals orphaned by a daemon restart become `expired`
rather than being silently revived.

The cross-SSH bridge uses the shared typed protocol in `../protocol/`.
`beamsd` generates a fresh `bridge_id` for each reconnect, sends its
authoritative request/service snapshots, and only then marks the bridge ready.
That synchronization barrier prevents a just-submitted request from being
overwritten by a late restart snapshot. A Beam can submit proposals and
observe decisions, but the bridge protocol has no VM-to-client approval
message.

Inside a managed Beam shell, `beams fetch` copies files or directories back
to the workstation:

```sh
beams fetch report.csv                  # pick the destination locally
beams fetch out/*.log ~/Downloads/logs/  # trailing argument: suggested destination
beams fetch --to ./build-output dist     # same, explicitly
```

Globs are expanded by the Beam's shell; a last argument that doesn't exist in
the Beam is the suggested destination (`--to` makes it explicit, and the Beam
shell's expansion of `~` is turned back into `~` for the workstation).
Nothing is copied until the workstation user approves the request in the local
menu, whose "Save to" field is prefilled with the suggestion, else the most
recently used directory, else `./` (the directory `beams ssh` was started
from), and has the path dropdown. One plain file lands at the destination (or
inside it, if that's a directory); several items, or a directory, land inside
the destination directory, which is created if needed. Each item is copied
with `tsh scp` (`-r` for directories) into a hidden sibling temp name and
renamed into place; existing targets fail the fetch unless `--overwrite` was
given. The command prints byte progress until the copy finishes and exits
non-zero on rejection, expiry, failure, or cancellation. Ctrl-C withdraws the
request: the Beam daemon notices the command's connection closing and tells
the workstation, which drops it if still pending or cancels it if running.

Fetch requests, the withdrawal message, and transfer progress are announced
by capability in the bridge hello (`file_fetch`). The Beam refuses `beams
fetch` with an "update your workstation" error when the connected workstation
predates them, rather than sending a message an older daemon would treat as a
protocol error.

The other request types:

```sh
beam-bootstrap client list
beam-bootstrap client request port-forward --remote-port 8080
beam-bootstrap client request upload \
  --remote-path /tmp/input.tar --local-path-hint input.tar
beam-bootstrap client request download \
  --remote-path /tmp/result.json --local-path-hint result.json
beam-bootstrap client request sync \
  --remote-path ~/projects/example --local-path-hint example
```

A direct SSH shell has no managed token and receives an actionable error. A
stale token is also rejected rather than falling back to another workstation.
The request expires after 60 seconds if no local frontend decides it. Approval
starts a listener bound only to `127.0.0.1`; explicit bind conflicts fail
before SSH is started, while an omitted local port is selected automatically.
The daemon owns the forward child and stops it during handled shutdown on every
supported Unix platform. Linux additionally uses `PR_SET_PDEATHSIG` so an
abrupt daemon crash cannot orphan the forward; macOS has no equivalent API.
Desired-running forwards are restored from typed persisted state after a
daemon restart without trusting an old PID. Stop/restart proposals from the
remote TUI require the same local approval as creation.

File copies support one regular file per job. They deny overwriting by
default; tick the overwrite toggle in the menu or pass `--overwrite` explicitly to
replace an existing destination. Uploads first write a unique sibling file
on the Beam, then atomically replace the destination or atomically create it
with a hard link for no-overwrite mode. Downloads use the same sibling-temp
pattern locally. Cancellation kills the active `tsh` child and removes the
temporary file best-effort, so a partial transfer is never installed as the
requested destination. On Linux, copy children use the same parent-death
protection as forwards. A daemon restart marks an in-flight copy interrupted
and does not retry it automatically.

Detailed workstation paths and transfer errors stay in local daemon state.
The Beam receives the typed direction, Beam path, optional display-only
basename hint, overwrite policy, and sanitized lifecycle text.

Folder syncs use Mutagen 0.18 or newer. `beamsd` creates a private
`~/.config/beams-client/mutagen` data directory and generated mode-0700
`ssh`/`scp` wrappers that dispatch Mutagen's transport through `tsh`; it does
not modify global SSH configuration or share the user's ordinary Mutagen
daemon. Both endpoint directories must already exist. The default is
`two-way-safe` with VCS metadata ignored. The local menu exposes all Mutagen
modes and ignore rules, with explicit warnings for local-wins and one-way
behavior.

Mutagen sessions are named deterministically from the Beam UUID and service
ID. Closing `beams ssh` leaves them running, and a handled `beamsd` shutdown
detaches its monitor instead of terminating the session. On daemon restart,
persisted syncs are found by name, their exact local/remote endpoints, mode,
and ignore settings are revalidated, and matching sessions are adopted.
Pause/resume preserves the session; terminate permanently deletes it.
Conflicts and disconnected endpoints are reported as `Degraded`.

## One-shot copy CLI

The same client-initiated copy path is available without opening the local
menu:

```sh
beams copy-to ./input.tar /tmp/input.tar
beams copy-from /tmp/result.json ./result.json

beams copy-to --purpose side-task --overwrite ./new.conf /etc/example.conf
```

`copy-to` takes `LOCAL_PATH REMOTE_PATH`; `copy-from` takes
`REMOTE_PATH LOCAL_PATH`. The Beam must already be active and tracked for the
selected `--purpose` (normally established by `beams ssh`), but an interactive
shell does not need to remain open. The command connects to or starts the
local daemon, prints lifecycle updates, blocks until the transfer completes,
and exits non-zero on failure, cancellation, or daemon disconnection.

## Folder-sync CLI

Create and manage the same sync services without opening the local menu:

```sh
beams sync create ./project ~/projects/project
beams sync create --mode one-way-safe --ignore target ./project ~/projects/project
beams sync list
beams sync pause SERVICE_ID
beams sync resume SERVICE_ID
beams sync terminate SERVICE_ID
```

`sync create` prints the service ID after Mutagen reaches a running, paused,
or degraded state. Pass `--include-vcs` to synchronize `.git` and other VCS
metadata; otherwise Mutagen's VCS ignore is enabled.

## Beam management wrappers

The rest of `tsh beams ...` is wrapped too, so managing beams doesn't
need copy-pasted ids. Anywhere tsh wants a beam id, these accept a
*target*: a beam id, a beam uuid, or a tracked purpose name (see
"State" below). Most default to the `default` purpose when no target is
given. `rm` never defaults, since it's destructive.

```sh
beams ls [--all]                    # beams + tracked purpose(s) + time left; flags stale purposes
beams rm <target>... [--prune]      # tsh beams rm, then forget purposes tracking it
beams rm --prune                    # just forget purposes whose beam is already gone
beams exec [target] [--login beams] -- <command>...
beams scp [-r] [-q] <src> <dest>    # remote operands are TARGET:PATH, or :PATH for `default`
beams publish [target] [--tcp]
beams unpublish [target]
beams ide [target] [--ide vscode|cursor|zed]
```

`rm` resolves every target before deleting anything, so a typo in one
doesn't leave the others half-done. `tsh beams add` isn't wrapped:
`beams ssh` (or `beams ssh --purpose <name>`) is the spawn path, since
it also tracks the purpose and bootstraps the beam.

## Configuration: SSH keys and startup tasks

Client-side environment profiles, batched uploads/packages, and pre/post
hooks are proposed in `../docs/designs/LAUNCH_CONFIGURATION_DESIGN.md`; they are not
implemented yet. The configuration below is what works today.

`~/.config/beams-client/config.toml` (re-read on use; a missing file means
the defaults below):

```toml
[ssh_agent]
# Keys from this terminal's SSH agent that Beams may *ask* to use
# (`beams agent-keys` lists them; `--allow` adds one). Empty or missing:
# Beams see no keys at all.
allowed_keys = ["SHA256:..."]
prompt_timeout = 60        # seconds to approve a request (max 110)

[startup]
# Sent to each new Beam once, the first time `beams ssh` connects to it.
# Default without this section: the Go and Rust toolchains.
tasks = [
  { template = "install-toolchain-go" },
  { template = "install-toolchain-rust" },
  { template = "clone-git-repo", params = { repo = "git@github.com:example-org/example-repo.git", dest = "~/projects/example-repo" } },
  { template = "clone-git-repo", params = { repo = "git@github.com:timothyb89/teleport-harness.git", dest = "~/projects/teleport-harness" } },
  { template = "clone-git-repo", params = { repo = "git@github.com:timothyb89/rdocker.git", dest = "~/projects/rdocker" } },
]

[startup.ssh_grant]
# Lets those startup clones use the key without a prompt: logins to these
# hosts (as your known_hosts names them, and verified against it) as
# `user`, for at most `max_uses` signatures (default: one per ssh clone
# plus two) within `window_secs` (default 600), ending early once startup
# finishes. Each use shows a toast; revoke it from the menu.
hosts = ["github.com"]
user = "git"
```

There's no private key on a Beam: its `SSH_AUTH_SOCK` relays to the agent of
the `beams ssh` terminal attached to it, and every other use of a key shows
an "SSH key request" toast. Press the menu key to review it: what the
workstation verified (host via your `known_hosts`, user, key) comes first,
then what the Beam reported (command, reason, task), marked unverified.
Deny is the default. With no `beams ssh` attached, requests are denied
without asking. See `docs/designs/SSH_AGENT_PROXY_DESIGN.md`.

While `[ssh_agent]` is unset and this terminal's agent has keys, `beams ssh`
offers once (a toast) to share one, and the home screen says so until you
share a key or choose "Don't share any" (`a` opens the choice).

Every decision (asked, signed, granted, denied, rejected, expired, failed)
is appended to `~/.config/beams-client/ssh-agent-audit.jsonl`, one JSON
object per line: time, Beam, request id, key fingerprint, the verified
operation, the grant used, and the Beam's unverified claims. It's `0600`
and rotates to `.1` at 4 MiB. For example:

```sh
jq -c 'select(.event == "signed" or .event == "granted") | [.time, .operation, .claimed.task_run]' \
  ~/.config/beams-client/ssh-agent-audit.jsonl
```

## State

No server-side way exists to label a beam, so `~/.config/beams-client/state.json`
tracks which beam id serves which `--purpose` locally. `default` is what
plain `beams ssh` reuses; pass `--purpose` to track a separate beam for a
side task (e.g. reviewing a specific PR) without it being mistaken for your
default one. This is a simple heuristic, not a full design - revisit if
purposes outgrow this.

## Building

```sh
cd client
cargo build --release
./target/release/beams ssh
```

Tests:

```sh
cargo test
./tests/managed-session-smoke.sh
./tests/client-bridge-smoke.sh
```

The managed-session smoke test is Unix-specific (`script(1)` + a fake `tsh`
fixture) and
exercises the compiled client inside a real controlling terminal: local menu
rendering (including an enhanced-keyboard Ctrl-] sequence), raw input around
the prefix, PTY size propagation, deferred remote output, and child exit-code
propagation.

The client-bridge smoke test runs both real daemons and the real
`beam-bootstrap client-bridge`, with only the outer SSH executable replaced by
a local fixture. It covers two independent workstation clients attached to
one Beam, exact token routing, rejection, a remote-initiated file download
with local path approval and path-redaction checks, snapshot synchronization,
and both VM- and workstation-daemon restarts. It needs permission to create
temporary Unix sockets.

Phase 7's opt-in OpenSSH multiplexing compatibility test must run on a
workstation with a normal `tsh` profile and a disposable or otherwise safe
Beam target:

```sh
tsh config > /tmp/beams-ssh-config
./tests/openssh-controlmaster-spike.sh \
  --config /tmp/beams-ssh-config \
  --target beams@BEAM_HOST_AS_MATCHED_BY_THE_CONFIG
```

It exercises command, PTY, concurrent, SCP, dynamic-forward, cancellation,
and master-recreation channels over one `ControlMaster`. See
`../docs/designs/OPENSSH_MULTIPLEXING_SPIKE.md` for limitations and the remaining
Teleport auth/MFA/recording matrix.
