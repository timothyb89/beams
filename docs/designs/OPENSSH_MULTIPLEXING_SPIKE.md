# Spike: OpenSSH multiplexing for client-owned services

Date: 2026-10-28. This is the Phase 7 follow-up to
`docs/designs/CLIENT_SERVICES_DESIGN.md`: determine whether one OpenSSH connection can
carry the interactive shell, client bridge, forwards, copies, and Mutagen
transport instead of starting a separate `tsh` process for each.

## Recommendation

Proceed with an optional `OpenSshTransport`, but keep `TshTransport` as the
automatic fallback until the workstation compatibility matrix below passes.
Do not pursue `russh` yet.

An OpenSSH master does not eliminate Teleport from the connection. It changes
the process model from one large `tsh` process per SSH activity to one
Teleport proxy helper and one small OpenSSH master per Beam:

```text
beamsd
  └─ ssh -MN (ControlMaster)
       └─ tsh proxy ssh (one underlying Teleport connection)
            ├─ bridge command channel
            ├─ port-forward channels
            ├─ scp/SFTP channels
            └─ Mutagen agent channels
```

On this Beam, the installed binary sizes are:

| Program | Size |
|---|---:|
| `tsh` | 137 MiB |
| `tbot` | 110 MiB |
| OpenSSH `ssh` | 1.1 MiB |
| OpenSSH `scp` | 268 KiB |

The meaningful saving is process startup and resident memory, not merely disk
space: each additional service opens a channel in the existing `ssh` process
without starting another Go `tsh` process. One proxy helper remains alive for
the underlying connection.

## What was verified in this environment

The pregenerated config at
`~/.teleport-bot/identity/tim-beams.beams.sh.ssh_config` successfully:

- ran `tbot ssh-proxy-command` as an OpenSSH `ProxyCommand`;
- reached the Teleport SSH proxy;
- validated the Beam's host certificate against Teleport's host CA;
- completed Teleport-side public-key authentication for the bot's allowed
  `core` principal.

The final channel was rejected by the Beam's inner SSH service because that
credential is not usable as a Beam OS login. Its certificate permits only
`core` and Teleport's internal join principal, while normal Beam access uses
the `beams` login.

The active identity that makes `tsh ssh beams@...` work is also unsuitable for
an honest OpenSSH test here:

- `tsh config` rejects identity-file sessions;
- its usable `beams` certificate is backed by a PIV key represented inside a
  Teleport identity bundle, not an OpenSSH private-key file;
- the key exposed through this session's SSH agent is a different ED25519 key
  and cannot sign for that ECDSA certificate.

This is an identity-export limitation of the tbot/PIV environment, not a
failure of ProxyCommand or ControlMaster. A normal workstation profile is the
intended test environment because `tsh config` emits matching OpenSSH key,
certificate, host-CA, and ProxyCommand settings there.

## Reusable workstation test

Generate an isolated config rather than modifying `~/.ssh/config`:

```sh
tsh config > /tmp/beams-ssh-config
./client/tests/openssh-controlmaster-spike.sh \
  --config /tmp/beams-ssh-config \
  --target beams@BEAM_HOST_AS_MATCHED_BY_THE_CONFIG
```

Depending on the host patterns emitted by the local `tsh` version, the target
may need the cluster suffix shown in that config. The script does not create
or remove a Beam. It uses a private short-lived control socket, creates one
temporary directory inside the selected Beam, and cleans both up on exit.

It verifies:

- master creation and `ssh -O check`;
- remote command and forced-PTY channels;
- concurrent long and short command channels;
- attachment when the caller supplies Mutagen's `ControlMaster=no`;
- upload and download through `scp`;
- dynamic `ssh -O forward` and `ssh -O cancel`;
- warm-channel latency;
- clean master shutdown and recreation.

Passing this script proves channel mechanics. It does not prove the
policy/lifecycle cases below.

## Proposed backend

`beamsd` should own masters, with one key per:

```text
(Teleport profile, cluster, Teleport user, Beam login, Beam UUID)
```

Use a mode-0700 runtime directory and a short hashed socket path. The latter
matters on macOS, where Unix-domain socket paths are relatively short. Start
the master with `ControlPersist=no`; the daemon, rather than an OpenSSH idle
timer, should own its lifecycle.

Operations map cleanly:

- command/bridge: `ssh -o ControlPath=...`;
- forward creation: `ssh -S ... -O forward -L ...`;
- forward removal: `ssh -S ... -O cancel -L ...`;
- copy: `scp -o ControlPath=...` (`scp -S` means “SSH program,” not socket);
- Mutagen: private `ssh`/`scp` wrappers that preserve its arguments while
  adding the isolated config and `ControlPath`.

Mutagen's `-o ControlMaster=no` does not prohibit using an already-existing
master; it only prevents that invocation from becoming a master. The
workstation script checks this explicitly.

The first implementation should multiplex daemon-owned bridges and services
while leaving the foreground interactive session on `tsh ssh`. Once session
recording, MFA, terminal fidelity, and reconnection are verified, the
interactive PTY can use the master too. This staged rollout captures most of
the process savings without putting the primary shell on the new backend
immediately.

Backend selection should be capability-based:

1. Generate/read an isolated `tsh config`.
2. Start and check a master noninteractively.
3. Use `OpenSshTransport` while healthy.
4. Fall back to `TshTransport` when config generation is unsupported,
   credentials require an unsupported key provider, or master startup fails.

Never weaken host verification or copy credentials into Beams state merely to
make multiplexing available.

## Remaining acceptance matrix

Run these on a real workstation before enabling the backend:

- macOS and Linux OpenSSH clients;
- ordinary local profile and hardware-backed/PIV profile, if supported by
  the generated config;
- cluster with per-session MFA required;
- expected Teleport audit events and session recording for multiple channels;
- certificate expiry while a master is alive, then `tsh login` and recovery;
- network loss, master death, Beam expiry, and daemon restart;
- real interactive zellij resize/redraw and signal behavior;
- existing port-forward, file-copy, and Mutagen end-to-end acceptance tests
  through the master.

Certificate expiry deserves an explicit policy decision. An already
authenticated SSH transport may continue accepting channels after the
certificate used to create it expires. If that conflicts with desired
reauthentication or per-session MFA behavior, cap master lifetime and recreate
it before certificate expiry, or exclude affected operations from
multiplexing.

## Why not `russh`

`russh` could remove the OpenSSH child but not the difficult Teleport-specific
work. A native backend would still need TLS-routing proxy integration,
Teleport host/user certificates, hardware-key signing, renewal and MFA,
session recording semantics, file transfer, forwarding, and robust PTY
behavior. OpenSSH already supplies the channel multiplexer and interoperates
with Teleport's supported ProxyCommand path. Reconsider a native library only
if measurements after this backend lands show the remaining one-master/
one-proxy process pair is a real problem.
