# SSH agent proxy: workstation-held keys with per-use approval

Status: 2026-11-04. Phases 0-3 and most of phase 4 done; the relay is the
default on new Beams (see "Phases" at the end). Builds on the bridge, request
queue, and toast/menu UI described in `docs/designs/CLIENT_SERVICES_DESIGN.md`.

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
4. looks the host key up in the *workstation's* `known_hosts` to get a
   hostname. Plain entries name the key directly. Hashed entries (Debian's
   `HashKnownHosts yes` default) can't be reversed: they can only *confirm* a
   candidate name, so the Beam sends candidates parsed from the requesting
   `ssh` command line (`[host]:port` spelling for non-22 ports). The Beam
   gets no leverage from this, because a candidate counts only if the
   user's own `known_hosts` matches it against the verified key.

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
- `tsh` on a Beam authenticates with the tbot identity file and never
  connected to the agent in the spike. `TELEPORT_ADD_KEYS_TO_AGENT=no` is
  only a precaution.
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
then, for a task of a running workstation-submitted manifest (by its
claimed `BEAMS_TASK_RUN`), the client that submitted it, otherwise the
only connected client, otherwise reject. The run tag is advisory, but
routing on it grants nothing: the chosen workstation still evaluates the
request. Agent requests use
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
and user, decision, and how it was made). Implemented as
`ssh-agent-audit.jsonl` (`agent::AuditEvent`); the format is in
`client/README.md`.

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
   Process  ssh -o SendEnv=GIT_PROTOCOL git@github.com git-upload-pack 'example-org/example-repo.git'
   Parent   git clone git@github.com:example-org/example-repo.git
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
    accident. Each ignored key restarts the 500ms, so the guard ends only
    after a pause in typing. The review only opens from the menu key
    (straight from the toast, or the home screen); the old
    `--approval-takeover` mode, which opened the menu on arrival, was
    removed because typing that kept going could open the review (`Enter`)
    and then approve it (`y`, or `Up` then `Enter`).
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
  { template = "clone-git-repo", params = { repo = "git@github.com:example-org/example-repo.git", dest = "~/projects/example-repo" } },
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

## Phase 0 findings (2026-11-04)

Spike code: `beams-protocol`'s `agent` module (wire parsing, no
dependencies) and `agent_crypto` module (feature `agent-crypto`:
session-bind verification, fingerprints, plain and hashed `known_hosts`
matching), plus a hidden `beam-bootstrap agent-proxy-spike` subcommand. The
driver is `bootstrap/tests/ssh-agent-proxy-spike.sh`. It ran on a fresh,
disposable Beam (Debian 13, OpenSSH 10.0p2, git 2.47) with generated keys,
an unprivileged `sshd` on 127.0.0.1:2200, and github.com, and passed all
24 checks. The verification half ran Beam-side only for convenience. The
modules are ready to be used from `beamsd` as designed.

What held up:

- **Host verification works end to end.**
  - Every ssh connection sent session-bind, and it verified.
  - Against OpenSSH 10 servers the client signs with the
    `publickey-hostbound-v00@openssh.com` method, whose signed data repeats
    the host key. Both methods are parsed, and the hostbound key is checked
    against the session-bind key.
  - github.com verified and was named through plain `known_hosts` entries.
    The local `sshd` was named through a *hashed* entry, using the
    command-line candidate.
- **Session-bind arrives before REQUEST_IDENTITIES**, on every connection.
  So the verified host is known even when only listing keys. A later rule
  could expose keys per host instead of per Beam.
- **Payload classification:**
  - user-auth (with the username);
  - `SSHSIG` namespace `file` (`ssh-keygen -Y sign`) and `git`
    (`git commit -S` with `gpg.format=ssh`).
  - Each `ssh` process opens its own agent connection. ProxyJump-style
    nesting produces separate connections, each with its own bind, and
    parallel logins don't interfere.
- **Refusals:** add identity, remove all, and lock are refused, and the
  upstream agent is untouched. A key that isn't allowlisted is never listed
  and can't be used to sign.
- **Context from `/proc`:**
  - Gathering it takes 0.1–0.3 ms, so doing it when the connection is
    accepted costs nothing noticeable.
  - For a git clone the chain is `ssh` ← `/bin/sh -c` (from
    `GIT_SSH_COMMAND`) ← `git clone …` ← caller. `SSH_KEY_REASON` and
    `BEAMS_TASK_RUN` are inherited by every process in it.
  - An ANSI escape in the reason was stripped.
- **Latency:**
  - `git ls-remote` waited through a 45s simulated approval without
    complaint; ssh and git have no agent timeout of their own.
  - The real limit is the server's `LoginGraceTime`. With it set to 10s,
    a 15s approval failed with "Connection closed by … port 2200".
  - github.com kept an unauthenticated connection open through a 120s
    stall before auth (the longest tried).
  - OpenSSH's default grace time is also 120s. So the 60s default
    `prompt_timeout` is safe, but approvals much past ~2 minutes will
    fail on the server side whatever the client allows.

Design adjustments:

- **Display:** the requesting `ssh` argv is long and mostly options. The
  review screen should show the destination and the remote command
  (`git-upload-pack '<repo>'`) first, and elide the options. The spike
  limits each argv element to 300 characters after sanitizing.
- **Prompt timeout:** keep `prompt_timeout` ≤ 90s by default and document
  the server grace-time ceiling. When a dialog expires it should say "the
  server may have already given up", not just "expired".
- **Falling back to key files:** `ssh`/`ssh-keygen` silently use a private
  key file next to a given `.pub` when the agent doesn't offer that key. The
  rollout must actually delete the staged `beams_id_ed25519` from existing
  Beams. Otherwise the prompts give a false sense of safety.

Not covered yet:

- RSA and ECDSA *host* keys weren't exercised at all: GitHub and the test
  `sshd` both negotiated Ed25519, and the unit tests only use Ed25519. Phase 2
  should add RSA/ECDSA session-bind fixtures, captured from
  `ssh -o HostKeyAlgorithms=…`.
- Host *certificates* (`*-cert-v01@openssh.com` host keys) don't parse as
  plain keys. They currently fall back to "verified key, unknown host", and
  need `@cert-authority` handling if anyone uses them.
- Context for a peer that has already exited (the `/proc` read races the
  peer's exit) wasn't exercised. Reading at accept time made it moot in
  every observed case.

## Phase 1 status (2026-11-04)

Implemented, plus the parts of phase 2 that were cheap given the phase 0
modules: `beamsd` already parses requests authoritatively and verifies
hosts against `known_hosts`, and the review screen keeps verified facts
apart from the Beam's claims.

- **Protocol:** capability `ssh_agent`, advertised in *both* hellos. The
  Beam's hello gained `capabilities`, so a new workstation never sends
  agent messages to an older Beam, whose strict bridge parser would drop
  the connection. Beam → client: `AgentSessionBind`, `AgentSignRequest`
  (raw bytes in base64, plus bounded `AgentContext`), `AgentConnClosed`.
  Client → Beam: `AgentIdentities` and `AgentResponse`.
- **Beam (`bootstrap/src/agentproxy.rs`):** listens on `ssh-agent.sock` next
  to the daemon socket.
  - Identity listings are answered from the keys the client pushed.
  - Session-binds are acknowledged at once and forwarded.
  - Sign requests wait up to 120s for the client's answer.
  - Requests are routed by `BEAMS_SESSION_TOKEN`, then (added later) by a
    running manifest's submitting client, or to the only connected
    capable bridge.
  - When a bridge goes away, its waiting requests fail immediately.
- **Workstation (`client/src/agent.rs`, `clientd.rs`):**
  - `[ssh_agent] allowed_keys` and `prompt_timeout` in `config.toml`, re-read
    on every use.
  - Each frontend reports its `SSH_AUTH_SOCK` through `EnsureBridge`. A
    signature uses the agent of the frontend that approved it.
  - Keys are pushed when a bridge connects and whenever a frontend attaches
    or detaches; no frontend means no keys.
  - Requests are denied without asking when the proxy is disabled, when no
    frontend is attached, when the key isn't allowlisted, when the payload
    isn't recognized, or when 8 requests are already pending.
  - Agent requests are kept in memory only (never in `daemon-state.json`).
    They appear as `ssh_agent` request records, so the existing toast and
    request list show them. Signing runs off the frontend's thread, and
    every request and decision is logged in `daemon.log`.
  - `beams agent-keys` lists this terminal's agent keys, marks the allowed
    ones, and shows how to allow one.
- **UI:**
  - The toast reads "SSH key request" and stays up for as long as the
    request can still be approved.
  - The review menu offers Allow once / Deny / Decide later, with **Deny
    preselected**.

Tests:
- Unit tests: 101 bootstrap and 143 client (including a full `beamsd` agent
  flow against a fake local agent).
- `client/tests/ssh-agent-smoke.sh`: 18 local end-to-end checks with real
  `ssh`/`git`/`ssh-keygen`, a real `ssh-agent` and `sshd`, the real bridge
  binary and both daemons. The host is named through a hashed
  `known_hosts` entry.
- A live run of `beams ssh` (driven through tmux) against a fresh Beam with
  a throwaway key:
  - over the real `tsh` bridge, the toast appeared over zellij;
  - the review screen showed the host key and the Beam's claims, with Deny
    preselected;
  - `y` let `ssh` log in, and a bare Enter denied ("agent refused
    operation").

Not done yet, and noticed during the live run:

- **Defaults:** nothing points the Beam's default `SSH_AUTH_SOCK` at the
  relay yet, and `bin/ssh-agent-init`, the unlock pane, key staging and
  `wait-ssh-agent-unlock` are unchanged. Changing those defaults is phase 4
  (or sooner, once the remaining phase 2 items land).
- **Menu rendering:** the menu strips the leading indentation from intro
  lines, so the "Reported by the Beam" lines aren't visibly nested. Phase 2
  should render that section with its own style rather than relying on
  spaces.
- **Remaining phase 2 items:** the input guard (keys ignored for about
  500ms), folding several identical requests into one, a countdown in the
  toast, an expiry message pointing at the server's grace time, and a
  separate audit log.

## Phases

0. **Beam-only spike.** *Done; see "Phase 0 findings" above.* A filtered listener in front of a throwaway local
   `ssh-agent` on a Beam. Check: session-bind parsing and verification, user-auth
   and `SSHSIG` parsing, `/proc` context and `BEAMS_TASK_RUN` attribution,
   `git`/`ssh` behavior with a 30-60s delay before the signature, and
   `tsh` with `TELEPORT_ADD_KEYS_TO_AGENT=no`. No bridge changes.
1. **Plumbing.** *Done; see "Phase 1 status" above.* Protocol messages,
   frontend `SSH_AUTH_SOCK` registration, allowlist, identity push, relay to
   the workstation agent. Every signature prompts, using the existing toast
   and review flow with a minimal screen. Deny when no frontend is attached.
2. **Context.** *Done.* Authoritative parsing, host verification against
   `known_hosts` (plain and hashed), the two-tier review screen,
   sanitization, a 500ms input guard (and SSH key requests are only approved
   from their own review menu), folding by reported parent process, a
   countdown in the toast. RSA (sha2-256/512) and ECDSA P-256/384/521 host
   keys verified against real OpenSSH (`bootstrap/tests/ssh-agent-hostkey-algos.sh`,
   captured fixtures in `protocol/tests/fixtures/`). The audit trail is
   `daemon.log` (one line per request and decision), plus (added later) a
   separate JSON-lines audit file, `ssh-agent-audit.jsonl`.
3. **Client-owned startup.** *Done.* `[startup]` in `config.toml`, sent once
   per Beam over `RunManifest` (persisted in `daemon-state.json`), with
   `wait-ssh-agent` added for ssh clones; `[startup.ssh_grant]` as designed,
   except that it matches the prompt's *verified host name* rather than host
   keys resolved when it's issued (hashed `known_hosts` can't be listed by
   name; the check is equivalent). Passive "SSH key used" toasts; grants are
   listed on the home screen with Revoke. `bin/beam-shell` no longer
   submits a manifest. Tested end to end in `client/tests/ssh-agent-smoke.sh`.
4. **Cleanup and rules.** *Mostly done.* The relay is the Beam's
   `SSH_AUTH_SOCK` everywhere; no key is staged; `ssh-agent-init`, the
   unlock pane and `agent.env` are gone; agents are told about
   `SSH_KEY_REASON` via `.claude/CLAUDE.md`; docs updated. Key onboarding
   (not in the original plan): `beams agent-keys --allow/--deny`, a one-time
   "share a key?" offer in `beams ssh`, and `a` on the home screen.
   Remaining: remembered approval rules ("allow github.com for 30m"), and
   `TODO.md`.

## Handoff notes (2026-11-04)

Open items are tracked in `TODO.md` ("SSH agent proxy"). This section is
what a new session needs in order to work on it.

### Code map

- `protocol/src/agent.rs`: agent wire format (messages, user-auth and
  `SSHSIG` payloads, session-bind), base64 helpers. No crypto.
- `protocol/src/agent_crypto.rs` (feature `agent-crypto`): session-bind
  verification, fingerprints, plain and hashed `known_hosts` matching.
- `protocol/src/lib.rs`: bridge messages (`Agent*`, `RunManifest`,
  `Manifest*`) and capabilities (`ssh_agent`, `startup_manifest`). Both
  hellos advertise capabilities; never send a capability's messages to a
  peer that didn't list it, because the bridge parser is strict and the
  bridge drops on an unknown message.
- `bootstrap/src/agentproxy.rs`: the Beam-side relay, `/proc` context, and
  the 16 KiB context budget that keeps a request under `MAX_FRAME_BYTES`.
  `daemon.rs` implements `AgentBridges` for `Hub` (routing) and
  `start_manifest`/`run_bridge_manifest`. `executor::run_tagged` and
  `current_task_run` provide `BEAMS_TASK_RUN`. `sshagent.rs` gives tasks
  the relay path.
- `client/src/agent.rs`: config (`[ssh_agent]`, written with
  `toml_edit`), local agent I/O, `BindStore`, `evaluate` (the
  authoritative check), prompt text, fold keys, `beams agent-keys`.
- `client/src/startup.rs`: `[startup]`, `GrantConfig`/`GrantRecord`,
  `manifest_tasks` (adds `wait-ssh-agent`).
- `client/src/clientd.rs`: `AgentRuntime` state. The `impl Hub` methods
  `handle_agent_sign`, `decide_agent`, `submit_startup`,
  `startup_accepted`, `sign_with_grant` and `end_grants_where`.
  Frontends report `ssh_auth_sock` in `EnsureBridge`. Agent records and
  grants live in memory only; `startup_submitted` is persisted.
- UI: `session.rs` (`request_menu` for agent requests, `grant_menu`,
  `agent_keys_menu`, `input_guard_until`, `offer_key_setup`),
  `session/notices.rs` (request, key-setup and "SSH key used" toasts),
  `session/ui.rs` (`describe_request`, the grant rows).

### Tests

- Unit tests: `cargo test` in `protocol` (with `--features agent-crypto`),
  `bootstrap` and `client`.
- `client/tests/ssh-agent-smoke.sh` (`CARGO=...` if cargo isn't on PATH):
  the full local end-to-end check. Real Beam daemon, real bridge, beamsd,
  a scripted frontend, `ssh-agent`, `sshd`, and a startup manifest whose
  ssh clone runs under the grant. About 1 minute (one check waits out the
  20s prompt timeout).
- `client/tests/client-bridge-smoke.sh`: bridge regressions (run it
  whenever the hellos or bridge messages change).
- `bootstrap/tests/ssh-agent-hostkey-algos.sh <beam-bootstrap>`: host key
  types against real OpenSSH. `bootstrap/tests/ssh-agent-proxy-spike.sh`:
  the phase 0 spike (run on a disposable Beam).
- Live test recipe that worked well: run `beams ssh` inside `tmux new-session -d
  -x 200 -y 50` (stdout must be a tty, so don't pipe it), with its own
  `XDG_CONFIG_HOME` and `XDG_RUNTIME_DIR` (a separate beamsd and purpose
  state) and a throwaway `ssh-agent` and key. Drive it with `tmux
  send-keys 'C-]'`, `y`, `Enter`, and read it with `tmux capture-pane -p`.
  For a login target, start an unprivileged `sshd` on the Beam that
  trusts the throwaway key. Stop that beamsd with `beams daemon stop
  --force` under the same XDG variables, and `tsh beams rm` the Beam.

### Gotchas found along the way

- `ssh`/`ssh-keygen -Y sign` silently use a private key *file* next to the
  `.pub` when the agent doesn't offer it. Tests must delete key files
  after `ssh-add`, or a "denied" key appears to work.
- `ssh-key` 0.6 can't produce RSA signatures (verifying them works), so
  RSA coverage uses captured OpenSSH fixtures.
- sshd keeps the *first* occurrence of a keyword, so appending
  `LoginGraceTime` to a config does nothing; use `sed` to replace it.
- The server's login grace time (OpenSSH default 120s; GitHub tolerated at
  least 120s) is the real limit on how long a prompt can wait.
  `prompt_timeout` is capped at 110s.
- Debian hashes `known_hosts`, so names can only be *confirmed* from
  candidates (taken from the `ssh` argv), never listed.
- Tests that set process-wide variables (`XDG_CONFIG_HOME`,
  `SSH_AUTH_SOCK`, `BEAM_BOOTSTRAP_SSH_*`) race other tests in the same
  binary. Keep them to one test, or share a lock.
- In shell harnesses, a bare `wait` also waits for background daemons and
  hangs, so wait on specific PIDs. `pkill -f <pattern>` can match the
  shell running it if that shell's own command line contains the pattern.
- `tsh beams exec` runs the command through the Beam's login shell
  (fish), so bash syntax must be wrapped in a script file. `tsh beams scp`
  takes one source per call.
