# Client-owned services and remote requests

Status: implementation plan; Phases 1-5 implemented and Phase 6 code complete
pending laptop validation, 2026-10-28.

This design covers workstation-owned features that operate across a Beam's
SSH connection: folder sync, port forwarding, and file transfer. It builds on
the Mutagen findings in `FOLDER_SYNC_SPIKE.md` and the VM-side daemon/task TUI
in `DESIGN.md`, but keeps a strict authority boundary between them:

> A Beam may request a service and observe its status. Only the user's local
> `beams` client may approve, create, change, or destroy that service.

These are **not** bootstrap task templates. A task runs on the Beam and is
owned by `beam-bootstrapd`; a client service runs on the workstation (or is a
workstation-owned SSH channel) and is owned by the local client.

## Feasibility and main decisions

This is feasible without replacing `tsh` or implementing SSH in Rust.

- `beams ssh` must stop `exec`ing directly into `tsh ssh`. It will supervise
  `tsh` under a local PTY, pass terminal bytes through normally, and intercept
  one configurable escape key to open a local service/approval UI.
- A lightweight per-user workstation daemon (`beams daemon`, referred to as
  `beamsd` below) will own long-running services, their registry, and one
  control bridge per connected Beam. Interactive `beams ssh` processes remain
  frontends; closing a terminal must not silently terminate a sync session.
- The client implementation will initially use separate `tsh` subprocesses for
  the interactive shell, control bridge, port forwards, copies, and Mutagen's
  transport. The foreground owns the interactive child; `beamsd` owns the
  others. Connection setup is paid once per long-lived operation and the
  folder-sync spike showed that established tunnel performance is ample.
- The transport will be hidden behind a small interface so an OpenSSH
  `ControlMaster` backend can be added after a focused compatibility spike.
  A native `russh` backend is not on the critical path: it would also need to
  reproduce or integrate Teleport proxying, user certificates, renewal/MFA,
  session policy, and failure behavior.
- The existing VM daemon remains the Beam-side rendezvous point. A new
  `beam-bootstrap client-bridge` process, started by `beamsd` over SSH, connects
  its stdin/stdout to the daemon's UDS. This avoids publishing a port or
  forwarding a workstation UDS into the Beam.
- The VM daemon holds only an ephemeral projection of client/service state.
  `beamsd` is authoritative and resends a full snapshot whenever a bridge
  reconnects.

## Goals

- The local user can open a palette during `beams ssh` and create, inspect, or
  stop folder syncs, local port forwards, and file-copy jobs.
- A user or agent inside a managed Beam session can submit the same typed
  request. The local UI shows the exact proposed operation and does nothing
  until the user approves it.
- A request made from an ordinary direct SSH session fails clearly when it is
  not associated with a live `beams` client session.
- Active services are visible from both the local palette and the existing
  remote TUI. The remote TUI can request a stop/restart, but that action is
  routed back through the owning client rather than performed locally.
- One workstation can manage many Beams, and one Beam can be connected to from
  many workstations. Ownership and routing must remain unambiguous.
- Syncs and forwards can outlive one interactive terminal, but never the local
  daemon that owns them (except Mutagen sessions that can be reconciled and
  adopted after a daemon restart).

## Non-goals for the first implementation

- Replacing `tsh` authentication or Teleport's SSH transport.
- Remote/reverse forwards, SOCKS proxies, public (`0.0.0.0`) listeners, or
  arbitrary command execution as generic service types.
- Automatically approving requests based on their origin inside the VM.
- Making a client-owned service durable on a different workstation.
- Perfectly compositing a local TUI over an arbitrary remote full-screen
  application. The first UI temporarily takes over the terminal, then restores
  and redraws the remote PTY.
- Treating a VM-supplied local path as authority. It is always a suggestion
  that the local UI resolves and confirms.

## Security and authority invariants

The VM, including `beam-bootstrapd`, its TUI, shell processes, and agents, is
untrusted input from the local client's point of view.

1. Every remote-created request starts in `pending_approval`. There is no
   "safe" service kind that bypasses approval.
2. Approval can only arrive over `beamsd`'s local UDS, whose permissions are
   limited to the workstation user. A message received over the SSH bridge can
   never represent approval.
3. `beamsd` parses typed protocol fields and constructs `Command` argument
   vectors. No VM-provided value is interpolated into a local shell command.
4. Local paths are expanded, canonicalized where possible, and shown in the
   confirmation dialog. File-copy requests from the VM cannot silently choose
   a local source or destination.
5. Local forwards bind `127.0.0.1` by default. Non-loopback listeners and
   reverse forwards are deferred until they have a separate, conspicuous
   permission flow.
6. Protocol messages have size limits, bounded pending queues, request
   timeouts, and per-bridge rate limits so a noisy VM cannot exhaust the local
   UI or daemon.
7. An optional future "remember this approval" rule must be scoped to a client
   ID, Beam UUID, service kind, normalized endpoints/path root, and an expiry
   no later than the Beam's own TTL. The initial implementation has no
   remembered approvals.
8. Remote status uses a user-approved display label for local paths rather
   than automatically exposing full workstation paths and usernames.

The managed-session token described below is for routing and for producing a
clear direct-SSH error. It is not a security boundary: code running as the same
user in the VM may be able to inspect another process's environment. Local
approval remains the security boundary even if the token is copied.

## Architecture

```text
workstation
┌─────────────────────────────────────────────────────────────────────┐
│ beams ssh frontend                                                  │
│   local PTY ↔ tsh ssh -t ↔ remote zellij                            │
│   hotkey palette / approval dialog                                  │
│             │ local UDS                                             │
│             ▼                                                       │
│ beamsd                                                              │
│   service registry + approval state + event bus                     │
│   ├─ Mutagen sync sessions                                          │
│   ├─ tsh ssh -N -L forward processes                                │
│   ├─ tsh scp / SFTP-style one-shot jobs                             │
│   └─ per-Beam bridge supervisor                                     │
│          │ long-lived `tsh ssh ... beam-bootstrap client-bridge`    │
└──────────┼──────────────────────────────────────────────────────────┘
           │ stdin/stdout NDJSON over authenticated SSH
Beam VM    │
┌──────────▼──────────────────────────────────────────────────────────┐
│ beam-bootstrap client-bridge                                        │
│             │ ~/.beam-bootstrap/daemon.sock                         │
│             ▼                                                       │
│ beam-bootstrapd                                                     │
│   request routing + ephemeral client/service projection + broadcast │
│      ▲                              ▲                               │
│      │ UDS                          │ UDS                            │
│ beam-bootstrap client request ...  beam-bootstrap tui               │
└─────────────────────────────────────────────────────────────────────┘
```

There are two separate SSH activities:

- The interactive PTY belongs to the foreground `beams ssh` process.
- The control bridge and long-running services belong to `beamsd`.

That split keeps terminal latency and lifecycle simple while allowing services
and pending requests to survive a terminal disconnect. A future OpenSSH master
may carry all of these as channels over one underlying connection without
changing either protocol.

## Identity, routing, and ownership

Use UUIDs rather than Beam aliases or process IDs:

- `client_id`: persistent random ID generated once per workstation/user and
  stored in the client config. A human-readable hostname is display-only.
- `beam_uuid`: Teleport's stable UUID for the VM. The randomized Beam name is
  display-only.
- `bridge_id`: one incarnation of a client-to-Beam bridge. Changes after a
  reconnect.
- `ui_session_id`: one interactive `beams ssh` frontend connected to `beamsd`.
- `managed_session_token`: random token passed only to the remote command
  environment for that managed shell and registered with its bridge.
- `request_id`: globally unique ID generated at the request's origin.
- `service_id`: globally unique ID generated by the owning `beamsd`.

Every service has exactly one `owner_client_id`. Other clients may display it,
but actions are routed to the owner. A remote request normally targets the
`client_id` associated with its managed shell token. If routing is absent or
ambiguous, it is rejected rather than broadcast to every workstation.

Expected direct-SSH error:

```text
no managed Beams client is associated with this shell
reconnect with `beams ssh`, then retry the request
```

If the shell has a stale token whose bridge is gone, the error instead names
the disconnected client and suggests reconnecting it. The remote CLI may have
an explicit `--client <id>` for recovery/debugging, but this does not bypass
local approval.

Multiple frontends connected to the same `beamsd` all receive a pending
approval event. The daemon performs an atomic pending-to-approved/rejected
transition, so the first decision wins and every other dialog closes with the
result.

## Protocols and state

Use versioned, newline-delimited JSON for both UDS protocols and the SSH bridge.
The existing daemon protocol can remain backward compatible by adding new
operations/events. Introduce a small path-dependency crate (for example
`protocol/`, package `beams-protocol`) for bridge message enums, service specs,
IDs, and status types consumed by both Rust binaries. Workstation-only UDS
messages may remain in `client/`. Do not pass untyped `serde_json::Value`
through the new control plane.

Every initial handshake includes:

```json
{"protocol":"beams-client-services","version":1,"role":"client","client_id":"...","bridge_id":"..."}
```

Unsupported versions fail before requests can be submitted. Each line is
bounded (proposed initial limit: 64 KiB).

### Local `beamsd` UDS

The local UDS supports:

- frontend registration/unregistration;
- list Beams, clients, requests, services, and recent jobs;
- submit a client-initiated service request;
- approve/reject a remote request;
- request stop/restart;
- subscribe to request/service/bridge events;
- ensure or reconnect a bridge for a Beam.

Client-initiated operations use the same validation and service creation path
as approved remote operations, but their trusted local origin means they enter
at `approved` rather than `pending_approval`.

### SSH bridge

VM-to-client messages are proposals or observations:

- `request_submitted`;
- `service_action_requested`;
- `bridge_heartbeat`;
- optional VM-side preflight results.

Client-to-VM messages are authoritative projections:

- request accepted/rejected/expired;
- full service snapshot;
- incremental service status;
- owning client/bridge status;
- errors safe to display remotely.

The bridge process itself contains no service implementation. It reconnects to
the VM UDS with backoff and forwards framed messages in both directions.
`beamsd` supervises and restarts the SSH process with bounded backoff. If `tsh`
needs interactive reauthentication, the bridge becomes `auth_required` and a
foreground client tells the user how to run `tsh login`; a background daemon
must not try to improvise an authentication prompt.

### VM daemon additions

Add operations similar to:

```json
{"op":"register_client_bridge","client_id":"...","bridge_id":"...","session_tokens":["..."]}
{"op":"request_service","request":{...},"session_token":"..."}
{"op":"list_client_services"}
{"op":"request_service_action","service_id":"...","action":"stop","session_token":"..."}
```

And broadcasts:

```json
{"type":"client_status","client_id":"...","status":"connected"}
{"type":"service_request_status","request_id":"...","status":"pending_approval"}
{"type":"client_service_status","service":{...,"status":"running"}}
```

The daemon keeps this projection in memory. On restart it is empty; connected
bridges re-register and send a full snapshot. Services whose owner disconnects
become `unknown` rather than falsely appearing stopped.

### State machines

Request:

```text
submitted → pending_approval → approved → starting → running
                         └────→ rejected
                         └────→ expired
approved/starting ────────────→ failed
```

Client-initiated requests begin at `approved`.

Service:

```text
starting → running → stopping → stopped
    │         │          └────→ error
    │         ├────────────────→ degraded
    │         └────────────────→ unknown (owner/bridge lost)
    └──────────────────────────→ error
```

One-shot copies use the same service record but normally move directly from
`starting` to `running` (with byte progress) to `stopped/completed`.

## Local terminal UI

The current `ssh_exec` handoff must become a supervised PTY session.

Normal mode is byte-for-byte passthrough:

- local stdin is placed in raw mode;
- bytes go to the child PTY except for the configurable escape key; the input
  router recognizes both its legacy control byte and enhanced Kitty CSI-u /
  xterm modifyOtherKeys representation, since remote applications such as
  zellij can enable those protocols in the local terminal;
- child PTY output goes directly to local stdout;
- terminal resize events update the child PTY, which `tsh` propagates to the
  remote PTY;
- exit status and common signals are propagated.

Use a prefix-style escape key (proposed default: `Ctrl-]`):

- one press opens the local palette;
- pressing it twice sends `Ctrl-]` to the remote side using the terminal's
  original encoding;
- the selected key and escape behavior are printed once on connect and are
  configurable.

A newly pending remote approval for the current Beam also opens the palette
automatically. This is a once-per-request attention event rather than a loop:
closing the palette without deciding leaves it closed. The takeover briefly
ignores already-buffered non-close input so text intended for the remote shell
cannot accidentally approve a request. Requests for other Beams are not shown
or surfaced by this session, and disabling the escape key disables automatic
takeover as part of strict passthrough mode.

While the palette is open, remote output is drained into a bounded buffer so a
chatty process cannot deadlock on a full PTY or consume unbounded memory. The
local UI temporarily clears and owns the current terminal screen and offers:

- active services for this Beam, with owner and status;
- create folder sync;
- create local port forward;
- copy file to/from Beam;
- pending remote approvals;
- stop/restart/details actions.

It deliberately does **not** enter another terminal alternate screen: the
remote zellij process normally already owns that mode, and alternate screens
are not safely nestable. On close, clear the palette and force a remote redraw
via two distinct PTY resize notifications (`SIGWINCH` propagation); a short
delay prevents the supervised SSH process from coalescing them. Each local
frame is fully cleared before repainting, and forms clear their modal area
before rendering. If a full-screen application still renders badly, offer a
configurable redraw byte (normally `Ctrl-L`) rather than always injecting input
into the remote session.

This is a modal takeover, not a terminal emulator. Rendering the remote screen
into a local ratatui widget would require a VT parser, mouse-mode translation,
clipboard/OSC policy, and substantially more compatibility work without
improving the service model.

## Service types

### Local port forward

Initial shape:

```text
local bind address: 127.0.0.1
local port: explicit or auto-selected
remote host: 127.0.0.1
remote port: required
```

Implementation: a supervised
`tsh ssh -N -L <bind>:<local-port>:<remote-host>:<remote-port> <login>@<uuid>`
child. `beamsd` owns the child, captures diagnostic output, reports its PID only
locally, and marks the service stopped when the child exits. Auto-selected
ports need a bind/preflight plus retry because handing an already-bound socket
to `tsh` is not available.

Do not use Mutagen forwarding: the spike measured materially lower throughput
than plain SSH forwarding. Reverse forwards, SOCKS, and public bind addresses
are separate future service variants with stronger confirmation text.

### Folder sync

Implementation: Mutagen sync sessions named from `beam_uuid` and `service_id`,
using generated `ssh`/`scp` wrappers that dispatch through `tsh`, as proven in
`FOLDER_SYNC_SPIKE.md`.

Initial defaults:

- VCS directories ignored (`--ignore-vcs`);
- `two-way-safe` mode, so pre-existing divergent files surface as conflicts
  instead of silently overwriting either endpoint;
- local endpoint is alpha;
- no automatic deletion or clearing of a pre-existing remote directory;
- conflict/status details available in both UIs.

The UI may offer explicit `local-wins` (`two-way-resolved`) and one-way modes,
with their overwrite/deletion semantics explained. "Make remote an exact
replica" must be a separate destructive workflow with a remote-path preflight
and confirmation; Mutagen's initial one-way reconciliation does not itself
purge all pre-existing beta-only content.

`beamsd` stores the Mutagen session name and periodically reconciles status
from Mutagen rather than treating the CLI process that created the session as
the service. On daemon restart it discovers matching names, validates their
endpoints, and adopts them back into the registry.

Repository seeding (`git bundle`/checkout followed by working-tree sync) is a
later optimization described by the spike. The first folder-sync slice should
require both directories to exist and should not combine sync correctness with
git-history transfer.

### File copy

Start with `tsh scp` as a one-shot job; add SFTP only if a multiplexed backend
makes it useful.

- Client-initiated upload/download may choose both endpoints locally.
- A remote upload request may suggest a remote destination, but the local user
  chooses the local source.
- A remote download request may name the remote source, but the local UI picks
  or confirms the local destination.
- Directories and overwrite behavior are explicit fields, not inferred shell
  flags.
- Copy progress should be emitted when the chosen transport exposes it; an
  initial indeterminate running state is acceptable.

### Service actions

Local actions are authoritative and can stop/restart immediately.

A remote TUI action creates a new typed action request routed to the owning
client. It never kills a workstation process itself. The initial policy should
require approval for both create and stop/restart requests; a future UI may
allow "remote may stop this specific service" as a per-service opt-in.

## Local persistence and recovery

`beamsd` is the only writer for service state. Keep the first implementation
consistent with the existing client state approach: atomically replace a small
versioned JSON file under the user's state/config directory rather than adding
a database.

Persist:

- `client_id`;
- service specification and owning Beam UUID;
- Mutagen session name;
- desired state;
- last known status/error and timestamps;
- user-approved display labels.

Do not trust persisted PIDs. After restart:

- Mutagen sessions are queried and adopted by name and validated endpoints.
- Plain `tsh -L` child processes are gone with the daemon and become stopped;
  restart only services whose persisted desired state is `running`, after
  checking the Beam still exists and the local bind is still safe.
- One-shot copy jobs that were in flight become failed/interrupted and are not
  automatically repeated.
- Every live bridge receives a new full snapshot.

When a Beam expires or disappears from `tsh beams ls`, stop its owned forwards
and syncs, terminate its bridge, and retain a short history record for the UI.

## Transport abstraction and connection reuse

Keep the interface narrower than an SSH library:

```text
open_interactive_pty(beam, remote_command)
spawn_bridge(beam, remote_command)
spawn_local_forward(beam, spec)
copy_to/copy_from(beam, spec)
mutagen_transport(beam) -> wrapper/config
```

The initial `TshTransport` implements each operation with a subprocess. This
ships the trust/UI/service model without first solving Teleport-specific SSH
multiplexing.

Then run an OpenSSH compatibility spike:

1. Generate an isolated SSH config from `tsh config` when running with a normal
   client profile, or construct its predictable Teleport `ProxyCommand`
   directly. `tsh config` is known to reject the tbot identity-file setup used
   inside Beams (`tsh config command is not supported with identity files`);
   `~/.teleport-bot/identity/tim-beams.beams.sh.ssh_config` is a working
   example whose `tbot ssh-proxy-command` can be reproduced without invoking
   `tsh config`. Do not modify the user's global SSH config.
2. Start one `ControlMaster`/control socket per
   `(Teleport profile, cluster, Teleport user, Beam login, Beam UUID)`.
3. Exercise an interactive PTY, remote bridge command, `-L`, `scp`/SFTP, and
   the Mutagen wrappers concurrently through that master.
4. Test master death/recreation, Beam expiry, certificate expiry, `tsh login`
   refresh, MFA-required clusters, and session recording/audit behavior.
5. Compare behavior and latency with the subprocess backend before making it
   the default.

If this works, `OpenSshTransport` can replace multiple underlying connections
without affecting protocols or service ownership. If not, separate long-lived
`tsh` processes remain a valid production design.

Only reconsider `russh` after the feature is working and profiling shows
subprocess/OpenSSH management is the actual limiting factor. A proof of concept
must demonstrate Teleport certificate/proxy compatibility, host verification,
reauthentication/renewal, PTY fidelity, forwarding, and file transfer before it
can replace `tsh`.

## Implementation phases

Each phase should land with docs, protocol fixtures, and unit tests before the
next one. Port forwarding is the first vertical service slice because it has
the fewest external lifecycle semantics; Mutagen comes after ownership,
approval, and recovery are already exercised.

### Phase 1: supervised interactive SSH and local palette

- Replace `CommandExt::exec` with a PTY-backed child while preserving exit
  status, resize, signals, raw-mode cleanup, and byte passthrough.
- Add the configurable escape key, modal ratatui shell, and forced-redraw
  behavior.
- Initially show static connection details so this phase does not depend on a
  daemon or service protocol.
- Test against a fake PTY child for binary passthrough, literal escape,
  resizing, child exit, and terminal restoration after errors.

### Phase 2: local daemon and typed protocol

- Add `beams daemon`, single-instance UDS binding, auto-start from frontends,
  persistent `client_id`, versioned handshake, subscriptions, and atomic state
  writes.
- Add typed workstation-only protocol/state modules; reserve the shared
  `beams-protocol` crate for the cross-SSH messages introduced in Phase 3.
- Move palette data/actions through the UDS.
- Add request/service state machines with deterministic transition tests.
- Add size/rate/time limits and socket-permission tests.

### Phase 3: bridge and remote request round trip

- Add `beam-bootstrap client-bridge` and VM-daemon bridge registration,
  snapshots, and broadcasts.
- Add the shared `beams-protocol` path dependency to `client/` and
  `bootstrap/`, with serialization fixtures proving both sides use the same
  wire shapes.
- Have `beamsd` supervise one reconnecting bridge per Beam.
- Pass a managed session token/client ID into the `beam-shell` remote command.
- Add `beam-bootstrap client list` and
  `beam-bootstrap client request port-forward ...`.
- Verify direct SSH and stale-token errors, rejection, timeout, bridge
  reconnect, VM-daemon restart, local-daemon restart, and multiple-client
  routing before creating a real service.

### Phase 4: local port-forward vertical slice

- Implement client- and remote-initiated local forwards through one creation
  path.
- Add approval details, process supervision, bind conflict handling, status,
  stop/restart, and diagnostic logs.
- Extend the VM TUI with `Tasks | Services | Requests` tabs and the service
  status/action protocol.
- Exercise two local frontends racing to answer one request and two different
  clients connected to the same Beam.

### Phase 5: file-copy jobs

- Add typed upload/download specifications and local path pick/confirmation.
- Implement one-shot `tsh scp` jobs, overwrite rules, cancellation, and
  interrupted-job recovery semantics.
- Add CLI equivalents for non-TUI client-initiated operation.

Implemented: the authoritative workstation path is retained only in local
daemon state; the Beam receives a display-only basename hint and sanitized
status text. Single-file uploads/downloads use sibling temporary files and
atomic install/no-clobber operations. Copy jobs are terminal and never
auto-restarted after daemon interruption.

### Phase 6: Mutagen folder sync

- Productize the spike's `ssh`/`scp` wrappers in a private client state
  directory.
- Add Mutagen availability/version checks with actionable installation errors.
- Implement create/list/monitor/pause/resume/terminate and daemon-restart
  adoption by deterministic session name.
- Expose mode, ignore rules, conflict state, endpoint validation, and explicit
  destructive options in the approval UI.
- Test pre-populated directories, conflicts, dropped SSH bridges, Beam expiry,
  local daemon restart, and a real laptop-to-Beam path before calling it done.

Implemented in `client/src/sync.rs` and the shared client-service protocol.
The workstation daemon stores the authoritative local path and deterministic
Mutagen session name, while the Beam sees only a display hint and its own
endpoint. A disposable-Beam run from this ephemeral Beam (`hidden-lab`,
removed immediately afterward) verified pre-populated-directory merge,
propagation in both directions, pause/resume isolation, `two-way-safe`
conflict reporting, daemon-restart adoption with exact endpoint/settings
validation, and explicit termination removing the Mutagen session. A real
laptop-to-Beam run and automated disconnected/expired-Beam cases remain the
acceptance checks before changing this phase to fully complete.

### Phase 7: multiplexing and hardening

- Run the OpenSSH `ControlMaster` spike and add the backend only if it passes
  the Teleport lifecycle matrix above.
- Add cleanup/expiry integration, structured diagnostics, bounded history,
  protocol fuzz/property tests, and end-to-end tests with disposable Beams.
- Revisit approval rules, reverse/public forwards, SFTP, and git-bundle seeding
  as separate opt-in features.

## Acceptance criteria for the first useful release

- During `beams ssh`, `Ctrl-]` opens a local palette without terminating or
  corrupting the remote session.
- A remote `request port-forward` produces a local confirmation dialog. Before
  approval, no local listener or SSH-forward process exists.
- Approval starts a loopback-only forward, and both local and remote UIs show
  its status and owner.
- Rejection, timeout, bridge loss, and direct SSH produce clear terminal
  states/errors.
- Closing the interactive terminal leaves the approved forward running under
  `beamsd`; stopping it locally updates the remote TUI.
- Two workstations connected to one Beam cannot accidentally manage each
  other's services.
- The same request/approval path can then be reused for copy and Mutagen sync
  without adding a new privileged VM-to-client command channel.
