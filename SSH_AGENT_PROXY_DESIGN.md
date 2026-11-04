# SSH agent proxy: workstation-held keys with per-use approval

Status: design, 2026-11-04. Not implemented. Builds on the bridge, request
queue, and toast/menu UI described in `CLIENT_SERVICES_DESIGN.md`.

## Problem

Today `client/src/stage.rs` copies `.ssh/beams_id_ed25519` onto every Beam,
`bin/ssh-agent-init` starts a plain `ssh-agent` there, and a dedicated zellij
pane prompts for the passphrase. After that the private key sits on the
Beam's disk, and the unlocked key sits in an agent socket under `/tmp` that
any process on the Beam (every coding agent included) can use silently, for
any host, for the Beam's whole lifetime.

Plain `ssh -A` forwarding would remove the key from disk but keep the second
problem: silent, unlimited use by anything that can reach the socket.

## Goal

- No private key material on the Beam, ever.
- Every signature is made by the workstation's own agent (whatever
  `SSH_AUTH_SOCK` the user's interactive `beams ssh` has), and only after the
  local user approves it in the `beams ssh` UI. The only exceptions are
  explicit, narrow, client-issued grants.
- The approval shows as much context as can be recovered, with what the
  workstation verified kept clearly separate from what the Beam claims.
- New-Beam startup clones still work without a prompt per repo.
- `git`, `ssh`, and `ssh-add -l` keep working unchanged on the Beam.

Non-goals (for now): GUI or OS-level prompts (the interface leaves room for
them, see "Approval surfaces"); signing anything other than SSH user
authentication and `SSHSIG` (git commit/tag signatures); key management from
the Beam (add/remove/lock are always refused).

## Threat model

The Beam is fully untrusted. As observed on a live Beam: the `beams` user has
passwordless sudo, Yama is not loaded (any process can ptrace any other
process of the same user), and `beam-bootstrapd`, its tasks, and every agent
all run as that one user. Anything the Beam reports can be forged, including
`/proc` contents, process ancestry, environment variables, task IDs, and the
daemon itself.

So:

1. **All policy is enforced in `beamsd`.** The Beam-side listener is a thin
   adapter, like `beam-bootstrap client-bridge`.
2. **`beamsd` parses the raw agent messages itself.** The Beam's parsing is
   used for nothing but display.
3. Context is in two tiers, shown separately in the UI:

| Verified by `beamsd` | Reported by the Beam (unverified) |
|---|---|
| Key fingerprint (must be on the allowlist) | Requesting pid, cmdline, cwd, parent chain |
| Signature kind: user auth vs. `SSHSIG` and its namespace | `SSH_KEY_REASON` |
| Remote **username** (parsed from the user-auth request being signed) | Which startup task the request belongs to |
| Remote **host** (session-bind host key → workstation `known_hosts`) | Repo name, fetch vs. push |

Remaining risk, stated plainly: approving one user-auth signature lets the
Beam complete one SSH login as that key. The agent cannot see or limit what
that session does afterwards (for example push instead of fetch). Limiting
what the key itself can do is the matching defense: a dedicated machine-user
key, deploy keys, or Teleport-brokered `tsh git` for orgs that support it.

### Verifying the host

OpenSSH ≥ 8.9 (Beams ship 10.0) sends `session-bind@openssh.com` on the agent
connection before asking for a user-auth signature. It carries the server's
host key, the session identifier, the server's signature over that
identifier, and an `is_forwarding` flag. `beamsd`:

1. checks the host key's signature over the session identifier;
2. records `(host_key, session_id)` for that agent connection;
3. on a sign request, parses the data as `SSH_MSG_USERAUTH_REQUEST` and
   requires its session identifier to match a recorded bind on the same
   connection;
4. looks the host key up in the *workstation's* `known_hosts` (including
   hashed entries) to get a hostname.

A signature produced this way only works in a session with that host key,
so even a compromised Beam cannot use an approval for "github.com" anywhere
else. Possible outcomes:

- `verified: github.com` — the bind checks out and the key is in `known_hosts`.
- `verified key, unknown host (SHA256:…)` — the bind checks out but the key
  isn't in `known_hosts`. The UI shows the fingerprint and the Beam-claimed
  hostname, marked unverified.
- `unverified` — no session-bind, or a mismatch. The request can only be
  approved once, by prompt: never by a grant or remembered rule.

`beamsd` does not forward session-bind to the workstation agent. Side
effect: keys added locally with `ssh-add -h` destination constraints will
refuse to sign. Document this rather than work around it.

## Architecture

```text
Beam
  ssh / git / ssh-add
      │ SSH_AUTH_SOCK=~/.beam-bootstrap/ssh-agent.sock
      ▼
  beam-bootstrapd: agent listener (new module)
      - one thread per agent connection, conn_id per connection
      - SO_PEERCRED pid → /proc context (advisory)
      - answers REQUEST_IDENTITIES from the client-pushed key list
      - refuses ADD/REMOVE/LOCK/UNLOCK/smartcard and unknown extensions locally
      - relays SIGN_REQUEST and session-bind over the existing bridge
      │ NDJSON, existing client-bridge
      ▼
Workstation
  beamsd: agent module (new)
      - authoritative parse + session-bind verification
      - policy: allowlist → grants → remembered rules → pending approval
      - forwards approved SIGN_REQUESTs to the approving frontend's agent
      │ local UDS
      ▼
  beams ssh frontend(s): toast → escape key → review screen
```

### Beam side

- The listener lives in `beam-bootstrapd` with a stable socket path
  (`~/.beam-bootstrap/ssh-agent.sock`, mode 0600). `bin/beam-shell`, the
  fish/bash config, and `environment.d` export `SSH_AUTH_SOCK` so
  noninteractive `tsh ssh` sessions get it too. `sshagent::discover_ssh_auth_sock`
  returns this path.
- `REQUEST_IDENTITIES` never prompts and never crosses the bridge per call:
  `beamsd` pushes the allowlisted public keys whenever the bridge connects
  and whenever they change. With no bridge connected the list is empty, and
  `ssh` falls through to its normal "Permission denied (publickey)".
- Context gathering happens when the connection is accepted, before
  anything is relayed: `/proc/<pid>/cmdline`, `cwd`, and the parent chain up
  to `beam-bootstrapd` or the session leader (at most 6 levels). For each
  level, cmdline plus `SSH_KEY_REASON` from that process's `environ`. If the
  peer has already exited, the context is reported as `unknown`; the request
  is not dropped.
- Task attribution: daemon-run tasks set `BEAMS_TASK_RUN=<manifest_id>/<task_id>`
  on the commands they spawn (`clone_git_repo` runs `git` as a direct child
  of the daemon). If the parent chain reaches the daemon's own pid with that
  marker set, the request is tagged with the run. This is advisory, like
  everything else here.
- Keep `TELEPORT_ADD_KEYS_TO_AGENT=no` in the Beam environment so `tsh` doesn't
  keep trying to add keys and failing.
- Response timeout: if `beamsd` hasn't answered within the request's
  `expires_at` plus a margin, the listener answers `SSH_AGENT_FAILURE`.

### Protocol additions (`beams-protocol`)

```rust
// Beam → client
AgentRequest {
    request_id: String,
    conn_id: String,           // stable per Beam-side agent connection
    raw: String,               // base64 of the full agent message (no length prefix)
    context: AgentContext,     // advisory, bounded, sanitized again by beamsd
}
AgentConnClosed { conn_id: String }

// Client → Beam
AgentIdentities { keys: Vec<AgentPublicKey> }   // blob + comment, allowlisted only
AgentResponse { request_id: String, result: AgentResult }
AgentRequestStatus { request_id: String, status: RequestStatus, message: Option<String> }

enum AgentResult { Success, Signature { raw: String }, Failure { reason: String } }

struct AgentContext {
    pid: Option<u32>,
    chain: Vec<ProcessInfo>,           // nearest first: { pid, argv (≤ 2 KiB), cwd, reason }
    task_run: Option<String>,
    managed_session_token: Option<String>,   // for routing only
}
```

`raw` stays opaque so parsing happens in exactly one place (`beamsd`). All
messages fit comfortably in `MAX_FRAME_BYTES`. Session-bind goes through
`AgentRequest` too. `beamsd` answers it with `Success` without a prompt,
after verification, and keeps the binding in per-`conn_id` state, dropped on
`AgentConnClosed` or when the bridge disconnects.

Routing follows the existing rules: the managed-session token if present,
otherwise the only connected client, otherwise reject. Agent requests use
the same per-bridge rate limits and pending-queue bounds as service requests,
with their own small cap (for example 8 pending per Beam) so a loop can't
flood the UI.

### Workstation side

**Which agent.** The rule is to use the user's current agent, with no
separate key store. Because `beamsd` is spawned once by the first `beams ssh`
(`clientd::spawn_daemon`) and survives self-restarts, its own inherited
`SSH_AUTH_SOCK` gets stale. So:

- Each frontend sends its `SSH_AUTH_SOCK` (if any) when it registers, and
  again on reconnect.
- A sign request goes to the agent of **the frontend that approved it**.
- Identity lists (and so the key list pushed to the Beam) come from the most
  recently registered live frontend for that Beam, refreshed whenever a
  frontend registers. A grant-approved request (no frontend involved) uses
  the same socket.
- If the socket is unreachable, the request fails with a message telling the
  user to check `ssh-add -l` in that terminal.

**Allowlist.** Only keys whose fingerprints are listed in the client config
are ever shown to a Beam. An empty allowlist means the feature is off. This
matters for a general-purpose local agent that also holds unrelated work or
personal keys.

**Parsing and checks** (in order; the first failure denies with a reason):

1. The message type is `SIGN_REQUEST` (or a session-bind extension, handled
   as above). Anything else is refused.
2. The key blob is on the allowlist.
3. The data parses as either
   - `SSH_MSG_USERAUTH_REQUEST` with service `ssh-connection`, method
     `publickey`, has-signature true, and the same key blob → `kind = auth`,
     plus the username and the session id; or
   - `SSHSIG` → `kind = sshsig`, plus the namespace (`git`, `file`, …) and
     the hash algorithm.
   Anything else is denied ("unrecognized signature payload"); no prompt
   offers to sign opaque data.
4. For `auth`, the host comes from session-bind matching (see "Verifying the
   host").
5. Policy lookup: grants, then remembered rules, then a pending approval.

**Policy outcomes.** Each request ends one of three ways: approved by a
grant (with a passive toast), approved or denied by the user, or denied
automatically (timeout, no frontend, policy). Each outcome is written to a
local audit log next to `daemon.log` (time, Beam, key, kind, verified host
and user, decision, and how it was made).

### Approval surfaces

For now: interactive `beams ssh` frontends only.

- **No frontend attached for that Beam → deny at once** with
  `"no interactive beams ssh session attached to approve"`, reported back to
  the Beam so the requesting terminal/log shows why. Nothing is queued.
- Agent requests are a new `RequestRecord` kind (`ssh_agent`) in the same
  pending queue, so `ToastPolicy` and the home-screen request list handle
  them with little new code. Differences from service requests:
  - The toast stays up while the request is pending (the caller is blocked),
    with a countdown, instead of `REQUEST_TOAST_FOR`'s 20s.
  - The default expiry is short (60s, configurable), because `git` is
    blocked waiting.
  - Toast text: `SSH key: github.com as git · <prefix> to review`.
- The review screen keeps the two sections visually distinct:

```text
 Use SSH key  beams_id_ed25519 (SHA256:abcd…)
 ── Verified by this workstation ──────────────────
   Sign in to   github.com  (known_hosts)  as  git
 ── Reported by the Beam (unverified) ─────────────
   Reason   Fetching the requested repository for review
   Process  ssh -o SendEnv=GIT_PROTOCOL git@github.com git-upload-pack 'gravitational/core.git'
   Parent   git clone git@github.com:gravitational/core.git
   In       ~/projects
 ──────────────────────────────────────────────────
  [ Allow once ]  [ Allow for this process ]  [ Allow github.com for 30m ]  [ Deny ]
```

  - All Beam-provided strings are stripped of control characters, ANSI/OSC
    sequences, and bidi overrides, and cut to a fixed width before rendering.
    A malicious `SSH_KEY_REASON` must not be able to draw a fake "Verified"
    line.
  - Input is ignored for about 500ms after the review screen opens, and the
    default focus is `Deny`, so keys typed into the shell can't approve by
    accident.
  - "Allow for this process" is limited to the reported root process
    (pid + start time) and the verified host/user. Because it relies on the
    advisory pid, it is offered only for verified-host requests and expires
    after 10 minutes at most.
  - "Allow host for N minutes" creates a remembered rule (below).
- Requests that arrive together with the same verified host, user, and
  reported root process (submodule fetches, `git fetch --all`) are folded
  into one entry: one decision covers them all.

GUI hooks for later: approval goes through a narrow `ApprovalSurface`
interface in `beamsd`, which takes a `PendingAgentApproval` view model (the
two tiers above, already sanitized) and returns a decision. The terminal
frontend is the only implementation today. "No surface available → deny"
is the general rule, so a later OS-notification or native-dialog surface
only has to register itself. The view model is serializable and shared with
the UDS protocol, so a GUI process can be just another frontend.

### Grants and remembered rules

Both live only in `beamsd` (persisted in `daemon-state.json`) and are shown
on the home screen with a revoke action. Neither can be created or widened
from the Beam. Both require `kind = auth` with a **verified** host (or
`sshsig` with an exact namespace, for remembered rules only).

```rust
struct AgentRule {
    beam_uuid: String,
    key_fingerprint: String,
    host_key: String,           // verified host key, not a hostname string
    username: String,
    expires_at: Timestamp,      // ≤ Beam TTL
    remaining_uses: Option<u32>,
    source: RuleSource,         // UserApproval | StartupGrant { manifest_id, task_runs }
    notify: bool,               // passive toast on each use
}
```

## Client-owned startup manifest and startup grant

The hard-coded repo list in `bin/beam-shell` moves into client
configuration, so the tool can be shared and each person can configure
their own repos and toolchains.

### Config

The client has no user config file yet (only `state.json` and
`daemon-state.json`). Add `config_dir()/beams-client/config.toml`:

```toml
[ssh_agent]
allowed_keys = ["SHA256:abcd..."]   # empty = proxy disabled
prompt_timeout = "60s"

[startup]
# Entries are passed through as `run_manifest` entries.
tasks = [
  { template = "install-toolchain-go" },
  { template = "install-toolchain-rust" },
  { template = "wait-ssh-agent" },
  { template = "clone-git-repo", params = { repo = "git@github.com:gravitational/core.git", dest = "~/projects/core" } },
]

[startup.ssh_grant]
hosts = ["github.com"]   # resolved through known_hosts to host keys when the grant is issued
user = "git"
window = "10m"
# max_uses defaults to the number of ssh-style clone entries + 2
```

### Flow

1. When `beamsd` brings up a bridge to a Beam and its persisted state
   (keyed by `beam_uuid`) shows no startup submission for that Beam, it
   submits `[startup].tasks` over the bridge (new `ClientToBeam::RunManifest`,
   which the bridge turns into the existing daemon `run_manifest` op).
2. The `accepted` reply's `manifest_id` and `tasks` plan come back over the
   bridge. `beamsd` records them and issues a startup grant (an `AgentRule`
   with `source = StartupGrant`) limited to:
   - this Beam;
   - the configured host keys and user;
   - `max_uses`;
   - `window`, ending early when that manifest reports `ok`/`error` or every
     ssh-style entry has finished;
   - requests whose advisory `task_run` names one of *those* run ids.
3. Each grant-approved signature shows a passive toast (`Startup: github.com
   as git (core) · 2 of 5 used`). A request that matches everything except
   `task_run` falls through to a normal prompt rather than a silent deny.
4. While the grant is active it appears in the home-screen list with
   "Revoke".
5. `bin/beam-shell` stops submitting a manifest. With no client connected,
   nothing is cloned (it would fail without a key anyway). An optional
   Beam-side default for SSH-free tasks (toolchains) can stay behind
   `BEAM_SHELL_SKIP_DEFAULT_TOOLCHAINS` until everyone's client config
   covers it.

Why it's built this way: being a startup task isn't something the Beam can
prove. Any process can submit a `clone-git-repo` task over the daemon UDS,
read a token from `/proc/*/environ`, or ptrace a task's `ssh`. The `task_run`
check only prevents *accidental* use (an agent's own `git clone` during the
window gets a prompt). What actually bounds the hole is enforced by the
client: verified host key, username, use count, and a short window, usually
before any agent has been started, with a visible toast on every use.

`wait-ssh-agent-unlock` becomes `wait-ssh-agent`: it waits until the bridge
has pushed at least one identity, still provides
`cap::SSH_AGENT_UNLOCKED`, and keeps the timeout.

## Removed or changed pieces

- `client/src/stage.rs`: stop staging `.ssh/beams_id_ed25519`.
- `bin/ssh-agent-init`, `bin/ssh-agent-init.fish`, the zellij `ssh-agent`
  pane, and `BEAM_SHELL_SSH_UNLOCK_DELEGATED`: removed.
- `bootstrap/src/sshagent.rs`: returns the fixed proxy socket.
- `bin/beam-shell` step 2.5 (default manifest): removed or reduced to
  SSH-free defaults (see above).
- The `claude_md` step on the Beam documents the proxy for agents:
  prefix SSH-using commands with
  `SSH_KEY_REASON="<one line, why>"` (e.g.
  `SSH_KEY_REASON="Clone repo the user asked to review" git clone …`),
  expect a human prompt, and treat a denial as final instead of retrying.

## Edge cases

- **Beam daemon restart**: the socket is recreated at the same path. Agent
  connections in progress fail and their pending requests expire.
  `beamsd` drops per-`conn_id` state when the bridge drops.
- **`beamsd` restart / self-restart**: pending agent requests are expired
  (as `expire_pending_after_restart` already does); grants and rules persist
  until they expire.
- **Multiple frontends for one Beam**: the first decision wins (existing
  atomic transition). The signature uses the deciding frontend's agent.
- **Local agent prompts itself** (`ssh-add -c`, hardware touch): this works,
  with two confirmations. The expiry clock pauses once the request has been
  approved and forwarded.
- **ProxyJump / nested sessions on the Beam**: multiple session-binds on one
  connection are recorded; the user-auth request's session id picks the
  right one. `is_forwarding = true` binds are shown, but never match grants.

## Phases

0. **Beam-only spike.** A filtered listener in front of a throwaway local
   `ssh-agent` on a Beam. Check: session-bind parsing and verification, user-auth
   and `SSHSIG` parsing, `/proc` context and `BEAMS_TASK_RUN` attribution,
   `git`/`ssh` behavior with a 30-60s delay before the signature, and
   `tsh` with `TELEPORT_ADD_KEYS_TO_AGENT=no`. No bridge changes.
1. **Plumbing.** Protocol messages, frontend `SSH_AUTH_SOCK` registration,
   allowlist, identity push, relay to the workstation agent. Every signature
   prompts, using the existing toast and review flow with a minimal
   screen. Deny when no frontend is attached.
2. **Context.** Authoritative parsing, host verification against
   `known_hosts`, the two-tier review screen, sanitization, input guard,
   folding, and the audit log.
3. **Client-owned startup.** `config.toml`, `ClientToBeam::RunManifest`,
   startup grant, passive toasts, `wait-ssh-agent`; trim `bin/beam-shell`.
4. **Cleanup and rules.** Remembered rules plus revoke UI, remove key staging
   and the unlock pane, update `AGENTS.md`/`bootstrap/README.md`/`TODO.md`.

Phase 0 can start now. Phase 1 touches `session.rs`/`notices.rs`
(new request kind) and `clientd.rs` (frontend registration), so it should be
coordinated with any concurrent UI work.
