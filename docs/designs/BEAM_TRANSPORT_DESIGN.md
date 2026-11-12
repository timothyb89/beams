# Beam transport: pinned endpoints and multiplexed connections

Date: 2026-11-12. Supersedes the recommendation in
`docs/designs/OPENSSH_MULTIPLEXING_SPIKE.md` (kept for its harness and
background). Covers every way the workstation client reaches a Beam:
the interactive session, the client bridge, port forwards, copies, Mutagen,
the git `ext::` push and `tsh beams ...` management calls.

## Problems

1. **Correctness: every call follows the current `tsh` profile.** No
   invocation passes `--proxy`/`--cluster`, so each one uses whatever
   `~/.tsh/current-profile` says *at that moment*. After
   `tsh login --proxy=other.example.com`, the bridge's next reconnect, a
   new forward, a copy or a Mutagen reconnect is routed to the wrong
   cluster and fails with "not found"/"access denied". `beams ls`/`rm
   --prune` then see a different cluster's Beam list and may wrongly
   treat tracked Beams as gone. `beamsd` is long-lived, so it can be
   started under one profile and keep running across several logins.
2. **Performance: one Go `tsh` process per SSH activity.** Each one is
   a full cold connection: process start, profile and key load, proxy
   TLS/ALPN, node dial and SSH handshake. Long-lived ones each hold about
   90 MiB.
3. **Prompts.** With hardware-key touch policies or per-session MFA,
   every cold `tsh` connection is another touch/tap. Today that's one
   per forward, copy, bridge reconnect and Mutagen reconnect.

## Measurements (this Beam → a disposable Beam, same region)

| Operation | Time |
|---|---:|
| `tsh version` (process start only) | ~85-90 ms |
| cold `tsh ssh beams@UUID true` | ~165-185 ms |
| cold `tsh beams exec ... true` | ~185 ms |
| cold `tsh scp` of a 3-byte file | ~200-280 ms |
| round trip on an already-open session | **2.2 ms** |
| proxy TCP connect / TLS handshake | ~6 / ~14 ms |

| Process | RSS |
|---|---:|
| long-lived `tsh ssh` | ~92 MiB |
| long-lived `tsh proxy ssh` (ProxyCommand relay) | ~93 MiB |

Roughly half of each cold call is CPU and process cost and half is about
10-15 network round trips. From a workstation with a 50-100 ms RTT to the
proxy, that is ~0.6-1.5 s per operation. A channel on an established
connection costs 1-2 RTTs and a `ssh` client start (a few ms). The
gap grows with distance, and most of it is removed by reusing the
authenticated SSH connection, not just the process or the proxy
connection.

## Where each prior-art option sits

```text
tsh process start ─► proxy TLS/gRPC ─► node dial ─► SSH kex+auth ─► channel
└──── tbot ssh-multiplexer amortizes ────┘
└───────────────── OpenSSH ControlMaster amortizes ──────────────┘
```

- **tbot `ssh-multiplexer` + `fdpass-teleport`** keeps a warm proxy
  client, cycled every 100 dials. For each request it dials the node
  fresh and passes the raw connection fd to OpenSSH (`ProxyUseFdpass`),
  so no relay process stays behind. Each OpenSSH invocation still does
  its own SSH handshake and auth with the node. It's designed for many
  short connections to *many* hosts, using a bot identity it holds
  in-process. Our pattern is many operations against the *same* one or
  two Beams, so it saves less than ControlMaster does here.
- **OpenSSH ControlMaster** keeps one authenticated connection per Beam;
  each operation is a channel. This is the ceiling for our workload. Its
  problem is credentials: OpenSSH must be able to authenticate.
- **VNet SSH** (Teleport Connect or `tsh vnet`, RFD 207) runs a local SSH
  endpoint at `<host>.<cluster>`. It accepts OpenSSH with a VNet-local key
  (`$TELEPORT_HOME/id_vnet`, `vnet_known_hosts`) and opens the upstream
  connection with the `tsh` profile's own credentials. It handles
  hardware keys and falls back to a per-session MFA cert. Teleport's docs
  name it as *the* supported way to use OpenSSH with per-session MFA. The
  cluster comes from the hostname, across all logged-in profiles, so it
  doesn't depend on `current-profile` at all. Layered under a ControlMaster,
  it gives one upstream connection per Beam with no relay process of ours.
- **`tsh config` + `tsh proxy ssh`** emits a ProxyCommand that already
  pins `--cluster` and `--proxy`, plus IdentityFile/CertificateFile for
  the profile key. This works only when that key is an ordinary file key.
  On this Beam the profile key is `-----BEGIN PIV YUBIKEY PRIVATE KEY-----`
  (a hardware-key reference): the ProxyCommand transport and host-CA
  check succeed, then OpenSSH fails with `Permission denied (publickey)`.
  Workstations with hardware-key policies will see the same result.

## Design

### 1. Pinned Beam endpoints (do first; fixes problem 1 by itself)

**Implemented 2026-11-12** in `client/src/endpoint.rs`. Differences from
the sketch below: the type is `Endpoint` (proxy, cluster, user,
`TELEPORT_HOME`), while login and UUID stay on the existing per-module
targets. Only `TELEPORT_SITE` is cleared; `TELEPORT_USER` doesn't affect
routing and is left alone. A purpose pinned to a logged-out cluster
other than the current one is reported, and `beams ssh` spawns a fresh
Beam on the current cluster instead of getting stuck. Mutagen's wrappers
read a per-host endpoint file, because the shared `mutagen daemon`
doesn't take environment from our calls. User-facing behavior is in
`client/README.md` ("Teleport profiles").

Record which Teleport endpoint each Beam belongs to when it's created or
first adopted, and route every call through it:

```rust
struct BeamEndpoint {
    proxy: String,                 // web proxy host:port of the profile
    cluster: String,               // cluster the Beam's node lives in
    user: String,                  // Teleport user, for diagnostics and keying
    teleport_home: Option<PathBuf>,// only if the user runs a non-default TELEPORT_HOME
    login: String,                 // OS login, `beams`
    beam_uuid: String,
}
```

- Store it in `PurposeEntry` (`state.json`). Fill it from
  `tsh status --format=json` immediately after `tsh beams add`, or on
  `--beam ID` adoption.
- Use one `tsh` command builder (`tsh::command(&endpoint)`) everywhere,
  replacing the separate `Command::new("tsh")` sites in `tsh.rs`,
  `bridge.rs`, `forward.rs`, `copy.rs`, `sync.rs` (wrapper env),
  `beamcmd.rs` and `clientd.rs`. It sets `TELEPORT_PROXY`,
  `TELEPORT_CLUSTER` and (if recorded) `TELEPORT_HOME`, and removes
  inherited `TELEPORT_SITE`/`TELEPORT_USER`. `--proxy`/`TELEPORT_PROXY`
  selects the profile by proxy host (`Config.LoadProfile(proxy)`), not
  `current-profile`.
- **Fail closed.** Verified: with `TELEPORT_PROXY` set to a different
  proxy, `tsh` returns `Not logged in` instead of silently using the
  current profile. Surface that as "log in to <proxy> again (`tsh login
  --proxy=<proxy>`)" instead of retrying blindly. The bridge backoff
  should treat it as a hard-down state with a toast.
- Make `tsh beams ls` per endpoint. Reuse, `ls` and `rm --prune` only
  compare a purpose against the Beam list of *its* proxy/cluster, never
  against whichever cluster is current. A purpose whose profile isn't
  logged in is reported as "profile logged out", not "beam gone".
- Legacy entries without an endpoint adopt the current profile on first
  use if, and only if, that profile's `tsh beams ls` contains the UUID.
  Otherwise they're reported as unknown and are never pruned.
- Mutagen's ssh/scp wrapper scripts already take `BEAMS_TSH`. Pass the
  pin through the wrapper environment too, so Mutagen reconnects stay
  pinned after `tsh login` elsewhere.

This alone fixes the reliability problem with no new transport and no
change to the process model.

### 2. A daemon-owned `BeamConnection` per endpoint

**Seam implemented 2026-11-12** (rollout step 2) in
`client/src/connection.rs`: one `BeamTarget` (ID, UUID, login,
endpoint) replaces the bridge/forward/copy/sync targets, and
`BeamConnection` has `exec`, `local_forward`, `copy`, `git_remote` and
`mutagen_host_entry`, with `TshConnection` (Tier C) as the only
implementation. `Hub::connection` in `clientd.rs` is the single place a
daemon worker gets one, which is where Tier B selection and the
per-endpoint master cache go. The CLI's git target preparation and push
use it too; `tsh beams add/exec/scp/ls` stay in `tsh.rs`, since they're
Beams management calls by Beam ID rather than SSH operations. Not yet in
the trait: `pty` (stage 4) and `health`/tier reporting (with Tier B).
`local_forward` still returns a long-lived process; Tier B can satisfy
it with a `ControlMaster=no` mux client or change it to `-O forward`.

`beamsd` owns one `BeamConnection` per `BeamEndpoint`, keyed by
`(proxy, cluster, user, login, beam_uuid)`. Every operation asks the
connection for a command instead of building `tsh ...` itself:

```rust
trait BeamConnection {
    fn exec(&self, remote_command: &str) -> Command;   // bridge, finalizers, git ext, mutagen agent
    fn pty(&self, remote_command: &[String]) -> Command; // interactive (stage 3)
    fn add_forward(&self, spec: &ForwardSpec) -> Result<ForwardHandle>;
    fn copy(&self, req: &CopyRequest) -> Command;      // scp now, tar stream later
    fn mutagen_env(&self) -> Vec<(OsString, OsString)>;// wrapper scripts + their env
    fn health(&self) -> Health;
}
```

Two implementations, chosen by capability probing (decided 2026-11-12):

| Tier | Transport | Works with | Notes |
|---|---|---|---|
| B | OpenSSH ControlMaster → `tsh proxy ssh --proxy=P --cluster=C` ProxyCommand, profile file key | file keys without per-session MFA | The fast path. One ~93 MiB relay per Beam. |
| C | pinned `tsh ssh`/`tsh scp` per operation (today's behavior, plus §1) | everything `tsh` supports | Always available; the fallback. |

The maintainer's cluster has no per-session MFA (login MFA only) and
VNet isn't in use, so Tier B covers the normal case. A VNet SSH tier
(called Tier A in the first draft) is deferred; see "Rejected or
deferred".

**Selection.** For each endpoint at master start:

1. **Tier B**, using a config generated from `tsh config --proxy=P` into
   the daemon's runtime directory. Never touch `~/.ssh/config`. Skip it
   immediately if the profile key file is a hardware-key reference
   (`PIV YUBIKEY PRIVATE KEY` header), or if a prior attempt for this
   endpoint failed at publickey auth or an MFA requirement. Cache that
   result until the next `tsh login` changes the profile's mtime. Start
   the master noninteractively and confirm with `ssh -O check`.
2. **Tier C** otherwise. Record the tier and the reason Tier B wasn't
   used, so `beams` can show it (`beams status`/the menu).

Never weaken host verification: Tier B uses `tsh config`'s
`UserKnownHostsFile` (Teleport host CA).

**Master lifecycle.**

- Spawn `ssh -F <private config> -MN -o ControlPath=<runtime>/m/<hash>
  -o ControlPersist=no -o ServerAliveInterval=15 -o ServerAliveCountMax=3
  -o ExitOnForwardFailure=yes -o BatchMode=yes beams@<uuid>.<cluster>` as
  a child of `beamsd`, with the same parent-death handling forwards use
  today. Keep the runtime directory mode 0700 and the socket name a short
  hash (macOS `sun_path` limit).
- Health: `ssh -O check` plus the child's exit. On exit, restart with
  backoff and fall back to Tier C for operations attempted meanwhile.
  Never let a client `ssh` silently open its own direct connection:
  clients get `-o ControlMaster=no` and are only handed out while the
  master is healthy.
- **Cert expiry.** An authenticated master can outlive its certificate,
  unless the role sets `disconnect_expired_cert`, in which case the node
  drops it at expiry. Read `valid_until` for the endpoint's profile.
  Shortly before it, stop handing out channels on the old master and
  start a new one; existing channels finish on the old master. If the
  new master can't authenticate (not re-logged in), report "log in
  again" just like §1 does.
- **Session caps.** Role option `max_sessions` limits sessions per
  connection. If a channel open is refused for that reason, start a
  second master for the endpoint instead of failing the operation.
- **Daemon restart.** On startup, adopt live sockets in the runtime
  directory (`-O check`) or close them with `-O exit`. Then re-establish
  forwards from persisted state, as today.

**Operations on a master.**

- exec/bridge/finalizers/git ext: `ssh -F cfg -S sock -o ControlMaster=no
  -T beams@host -- <one quoted command>`. Keep the single-argv quoting
  rule from `tsh::prepare_git_target`; OpenSSH has the same
  argv-rejoin behavior.
- Forwards: `ssh -S sock -O forward -L 127.0.0.1:L:host:R` and
  `-O cancel`. No per-forward process at all, and Teleport still sees
  ordinary `direct-tcpip` channels, so port-forwarding RBAC and audit
  apply.
- Copies: `scp -F cfg -o ControlPath=sock`. Then add the TODO's `tar`
  stream over one exec channel for multi-file fetches; with a warm master
  that's one channel instead of one cold `tsh scp` per file.
- Mutagen: the wrappers become `exec ssh -F cfg -S sock -o ControlMaster=no
  "$@"`/`scp ...`. Mutagen's own `-o ControlMaster=no` already means
  "don't become a master", not "don't use one" (the spike harness checks
  this).
- Interactive session: stage 3, below.

### 3. Rollout

1. **Pinning (§1).** Endpoint in state, single `tsh` builder, per-endpoint
   listing, fail-closed errors, legacy adoption, wrapper env. Tests: the
   fake `tsh` fixture asserts `TELEPORT_PROXY`/`TELEPORT_CLUSTER` on every
   call; a smoke switches the fake's "current profile" mid-session and
   expects the bridge and a new forward to stay on the original.
2. **`BeamConnection` with Tier C only.** Refactor bridge, forward, copy,
   sync and git through the trait with no behavior change. This is the
   seam the other tiers need, and it keeps the four separate `Target`
   structs from drifting.
3. **Tier B for daemon-owned work** (bridge, forwards, copies,
   Mutagen, finalizers, git). Run `openssh-controlmaster-spike.sh` from
   macOS and Linux workstations against a pinned `tsh config --proxy=P`.
4. **Interactive session over the master** (`ssh -t -S sock`, with
   `EscapeChar none`) once recording, resize, signals, zellij and MFA
   behavior are verified. Until then it stays `tsh ssh -t`, pinned.
5. **Optional.** Pre-warm the master as soon as a Beam is known (before
   the interactive session starts) so the first bridge connect and the
   upload happen on a warm connection. Use the master for launch uploads
   too (`TODO.md` Phase 6).

## Rejected or deferred

- **ControlMaster over VNet SSH.** It's Teleport's supported way to use
  OpenSSH with hardware keys and per-session MFA, and it routes by
  hostname rather than current profile. Deferred because neither applies
  to the maintainer's cluster and it means running VNet on the
  workstation. It would slot in ahead of Tier B behind the same probing
  (`$TELEPORT_HOME/vnet_ssh_config` exists only while VNet runs) if
  hardware-key or MFA roles appear.

- **A tbot-style fdpass multiplexer for user profiles.** It needs a Go
  helper linking Teleport's AGPL `lib/client` to hold the user's
  credentials. It would roughly match `tsh`'s size, and still
  re-authenticate every SSH connection. Against ControlMaster it would
  save only the master's own startup and its relay process. Reconsider
  only if Tier B's relay memory matters in practice.
- **Application-level multiplexing over the bridge's `tsh ssh` stdio.**
  Fast, and it works with any credential, but tunnelling TCP and files
  inside an exec session bypasses Teleport's port-forwarding RBAC, file
  transfer controls and audit events. Not acceptable as a general
  transport.
- **An ssh-agent shim that signs through Teleport's hardware-key agent
  (`tsh piv agent` gRPC `Sign`).** It would make Tier B work with PIV
  keys, but it's unsupported territory with touch/PIN edge cases. VNet
  SSH (above) is Teleport's supported answer for exactly this.
- **`russh`.** Unchanged from the spike: it doesn't remove the
  Teleport-specific credential work.

## Testing limits on a Beam

Inside a Beam the profile key is a hardware-key reference, so Tier B is
impossible there and development only exercises Tier C plus the
fake-`tsh` smokes. Benchmarks and Tier B's end-to-end checks must run
from a workstation.

`tsh ssh` from a Beam to *another* Beam works, but the delegated identity
is denied SSH into its *own* Beam. That works from a workstation. The
node logs (`component: node:forward`, `srv/authhandlers.go`) show the
node's access checker refusing the `beams` login:
`user bot-beam-<id>@<cluster> is not authorized to login as
beams@<cluster>: access to node denied`. It looks like a Beams
self-access quirk with the flattened delegated identity, worth raising
upstream (`TODO.md`).
