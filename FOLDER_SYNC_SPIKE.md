# Spike: cooperative folder sync over the Teleport SSH tunnel

Date: 2026-10-22. Goal: sanity-check whether `beams` <-> `bootstrap` folder
sync (large repos like Teleport `core`, ~2 GiB) is viable over the existing
Teleport-proxied SSH transport, and whether Mutagen is a good fit as the
sync engine. This supersedes nothing; it's input for a later design in
`DESIGN.md`/`TODO.md`.

## Setup

- Two beams in `us-west-2`: this session (`astral-hatch`) acting as the
  "local" side, and a throwaway beam `pure-cosmos` (spun up with `tsh beams
  add`, torn down with `tsh beams rm` at the end) acting as the VM side.
- Transport under test: `tsh ssh beams@<node-uuid>` (Teleport proxy +
  cert auth), both for raw TCP tunneling and as Mutagen's SSH transport.
- Repo under test: `~/projects/core`, 1.9 GiB total, 273 MB / ~18k files
  excluding `.git`.

**Caveat up front:** both endpoints are cloud VMs in the same region as the
Teleport proxy. This is a best-case topology — low RTT, no consumer-grade
uplink, no NAT/firewall weirdness. A real "local machine" client (laptop on
home wifi/VPN) will see materially worse numbers, especially for the
per-connection setup latency. Treat the throughput numbers here as a
*ceiling*, not a typical case.

## 1. Raw TCP tunnel performance

Forwarded a local port to a remote `iperf3` server through
`tsh ssh -L 15201:localhost:5201 beams@<uuid>`:

| Direction | Throughput |
|---|---|
| local → remote | 2.34 GBytes / 10s ≈ **2.39 Gbit/s** |
| remote → local (reverse) | 2.60 GBytes / 10s ≈ **2.24 Gbit/s** |

Per-connection setup overhead (`tsh ssh beams@<uuid> -- true`, cold cert/
proxy dial each time): **~250–340 ms**, measured over 5 runs. This is the
cost of spinning up a *new* SSH session each time, not a cost paid by data
already flowing over an established tunnel.

Conclusion: the tunnel itself is not the bottleneck. Teleport's proxying
adds negligible overhead vs. raw bandwidth — multi-gigabit throughput is
available once a session is up. The real cost is per-connection setup
latency, which matters for anything that opens lots of short-lived
connections (e.g. naive per-file transfers) but not for a long-lived sync
session or multiplexed port forward.

## 2. Mutagen over the `tsh ssh` transport

Mutagen doesn't support an arbitrary ProxyCommand out of the box, but it
does respect `MUTAGEN_SSH_PATH`, which lets you point it at a directory
containing drop-in `ssh`/`scp` replacements. Wrote two ~30-line wrapper
scripts that strip the OpenSSH flags Mutagen adds (`-o ControlMaster=no`,
`-p`, etc.) and re-dispatch through `tsh ssh <dest> -- <cmd>` /
`tsh scp <from> <to>`. This is enough for Mutagen to auto-install its own
remote agent binary and run normally — no manual remote install needed.

Scripts are throwaway (`/tmp/spike-tools/bin/{ssh,scp}`, not committed) but
the approach is the key finding: **any tool that shells out to a vanilla
`ssh`/`scp` binary can be made to ride the Teleport tunnel with a thin
wrapper and `$MUTAGEN_SSH_PATH`-style injection (or `GIT_SSH_COMMAND`,
`RSYNC_RSH`, etc. for other tools).** No Teleport-side changes needed.

### a) Initial full sync

| Scope | Size | Time to converge |
|---|---|---|
| Working tree only (`--ignore-vcs`) | 273 MB / 17,977 files | ~2.75–3 s |
| Full repo incl. `.git` | 2.0 GB / 18,003 files | **18.5 s** |

18.5s for 2 GB ≈ 108 MB/s (~865 Mbit/s) effective — well below the raw
2.3 Gbit/s tunnel ceiling, because Mutagen's cost here is dominated by
scanning/hashing/compressing tens of thousands of files, not link
bandwidth. Still fast enough that a first-time sync of `core` is a
"grab coffee," not "go to lunch," operation.

Practical note: Mutagen's own `--ignore-vcs` is the right default for this
use case. `.git` history should almost certainly be handled by `git` itself
(clone once, `git fetch`/`pull` thereafter) rather than pushed through a
generic file-sync engine — Mutagen reports packfiles as opaque blobs and
will re-transfer/re-hash them wholesale on any change, which is wasteful
for an append-mostly data store that already has an efficient native sync
protocol.

### b) Incremental single-file-edit sync

Appended one line to a tracked `.go` file, had a remote-side poll loop
(persistent SSH session, 50 ms poll interval) record when the change
landed:

**~260 ms** from local save to remote file content matching.

This is small enough that it's dominated by Mutagen's own filesystem-event
debounce, not tunnel latency — i.e., it should feel "instant" for normal
edit-save-rebuild workflows, even with the cold-connection-setup caveat
above in mind (the sync session itself is long-lived; it isn't paying that
setup cost per edit).

## 3. Bonus: Mutagen's port-forwarding feature

Tested `mutagen forward create tcp:localhost:15301
beams@<uuid>:tcp:localhost:5301` against a second `iperf3` server, through
the same wrapper transport:

**~120 Mbit/s** — an order of magnitude worse than both the raw `ssh -L`
tunnel (2.3+ Gbit/s) and Mutagen's own sync transfer (865 Mbit/s). Mutagen
forwarding clearly isn't doing a raw socket splice; it's relaying through
its own framed protocol, and that framing has real overhead. If we want to
expose port-forward management as a feature, **don't use Mutagen's forward
sessions for it** — a thin wrapper that drives plain `tsh ssh -L`/`-R` (or
`tsh proxy ssh` as an OpenSSH `ProxyCommand`, which `tsh config` normally
emits — not available here since this profile uses an identity file) gets
the real tunnel throughput and is simpler to reason about than adopting a
whole second Mutagen session type just for forwarding.

## Takeaways for later integration design

1. **Transport is a non-issue.** The Teleport-proxied SSH tunnel sustains
   multi-gigabit throughput between same-region beams and sub-second
   incremental sync latency. Even allowing significant derating for a real
   client on a home/office connection, this should comfortably support
   interactive "edit locally, build/test on the beam" workflows.
2. **Mutagen is viable as the sync engine** and needs no changes to how
   beams currently authenticates — it auto-deploys its own remote agent
   over the existing `tsh ssh`/`tsh scp` primitives once pointed at them via
   a small wrapper (`MUTAGEN_SSH_PATH`). That wrapper (or an equivalent
   `ProxyCommand`-based one, if we can get `tsh config`-style identity
   files working for that) is the one piece of client-side glue this would
   need; it's small and self-contained.
3. **Exclude `.git` from sync scope**; let git handle git. Mutagen syncing
   the working tree (and ignoring VCS dirs) is fast and simple; let
   `bootstrap`/`beams` do the initial `git clone`/`fetch` as it already
   does for repo delivery (see recent "Switch repo delivery to a git push
   transport" work).
4. **Don't reuse Mutagen's `forward` sessions for port-forward management**
   — build that on plain SSH forwarding instead, it's ~20x faster here and
   is also more composable with whatever CLI/TUI surface we want to expose
   port forwards through.
5. Numbers here are a *same-region VM-to-VM ceiling*. Before committing to
   a design, worth re-running steps 1–2 from an actual laptop (different
   network, higher/variable RTT, maybe behind corporate egress filtering)
   to see how much the picture changes — particularly the per-connection
   setup latency (~250-340ms here) which could matter more over a slower
   last-mile link, and whether `tsh`'s own dial/relay path adds meaningfully
   more overhead from outside the cloud.

## Cleanup

Spike beam `pure-cosmos` and all Mutagen sessions were terminated at the
end of the spike; nothing was left running or committed from the throwaway
wrapper scripts.

---

## Follow-up (same day): reconciling a pre-existing clone, and faster `.git` seeding

Prompted by: the VM can clone `core` directly from GitHub, but it's slow
(~120s, capped around 25 MB/s, not counting delta resolution) and can
never reach a state that includes the client's uncommitted/unpushed work.
Used a second throwaway beam (`emerald-circuit`, torn down after) for
these tests.

### Mutagen reconciling pre-populated, divergent directories

Tested all three relevant sync modes by seeding fake "alpha" (client) and
"beta" (VM) directories with: one file present on both sides but with
genuinely different content (simulating an uncommitted local edit vs. a
stale VM clone), one file that exists only on alpha, one that exists only
on beta.

| Mode | Divergent file | Alpha-only file | Beta-only file |
|---|---|---|---|
| `two-way-safe` | **left untouched on both sides**, reported under `Conflicts:` in `mutagen sync list` | copied to beta | copied to alpha |
| `two-way-resolved` | **alpha's content wins**, silently, no conflict reported | copied to beta | copied to alpha |
| `one-way-replica` | **alpha's content wins**, beta overwritten | copied to beta | *left alone* (see gotcha below) |

This directly answers questions 1 and 2:

- **Yes, Mutagen handles the "already cloned, now reconcile" case
  natively** — it's a content-hash-based scan/reconcile, not a git-aware
  diff, so it doesn't care that beta's copy came from a `git clone` and
  alpha's came from a live working tree. Anything identical on both sides
  is a no-op; anything different gets handled per the mode above.
- **Yes, there's a notion of "authoritative" endpoint, and it's `alpha`
  (whichever path you list first).** `two-way-resolved` auto-resolves any
  content conflict in alpha's favor with no prompting — exactly the "my
  local copy wins if it was dirty when the VM finished its clone"
  semantics you want. `two-way-safe` is the more conservative version of
  the same idea: it refuses to guess and just leaves both copies alone,
  surfacing the conflict for a human (or a later automated pass) to
  resolve. Point `alpha` at the client path and this just works — no
  extra flag needed to mark "authoritative."
- **Gotcha:** none of the two-way modes, nor `one-way-replica`, purge
  pre-existing beta-only content at session creation — `one-way-replica`
  only enforces mirroring (including deletions) for content it tracks
  going forward (confirmed: deleting a file from alpha *after*
  convergence correctly deleted it from beta). A file that was already
  sitting on beta before the session started and is never touched on
  alpha will simply sit there forever, in every mode. If you want "wipe
  the VM's copy to exactly match the client," `rm -rf` the destination
  before creating the session rather than relying on `one-way-replica` to
  do it.

Practical shape this suggests for the task runner: do a cheap `git fetch`
+ `git checkout <same-ref-as-client>` on the VM first (metadata-only,
fast, gets branch/HEAD aligned — and this is the one case the GitHub-based
clone *can* do correctly), then hand off to Mutagen in `two-way-resolved`
mode (alpha = client) scoped to the working tree (`--ignore-vcs`) to
reconcile any uncommitted/untracked files the branch checkout can't
capture. Mutagen never needs to touch `.git` for this to work.

### Faster `.git` seeding than a GitHub clone

Compared three ways to get a fully valid, non-shallow `.git` onto the VM,
all going over the same `tsh ssh` tunnel used throughout this spike,
transferring the real `core` repo's `.git` directory (1.6 GB):

| Method | Time | Effective throughput |
|---|---|---|
| `git clone` from GitHub (reported baseline) | ~120s | ~25 MB/s (capped) |
| Mutagen full-tree sync (from spike 1, 2.0 GB incl. `.git`) | 18.5s | ~108 MB/s |
| `tar \| ssh \| tar -x`, no compression | **11.0s** | ~145 MB/s |
| `tar \| zstd -1 \| ssh \| zstd -d \| tar -x` | **9.6s** | ~167 MB/s |

Verified the tar-seeded `.git` is a fully valid repo on the far end:
correct branch, correct `git log`, and `git checkout .` cleanly
materialized all 17,997 working-tree files.

Takeaways:

- **A raw `tar`-over-the-tunnel pipe of `.git` beats both the GitHub clone
  (~11x) and Mutagen's own file-by-file sync (~1.7x)** for this use case,
  because it's one continuous stream instead of per-file scan/hash/stage
  overhead, and it isn't rate-limited by GitHub.
- **Client-side compression is a minor win, not a major one (~13%
  faster), because git's own data is already compressed** (zlib in loose
  objects and packfiles) — most of what `zstd` can additionally squeeze
  out is refs/logs/index, a small fraction of the total. It's a free
  win given spare CPU, but don't expect it to be the dominant lever;
  burning a lot of client CPU on heavier compression levels isn't likely
  to pay for itself when the tunnel itself isn't the bottleneck.
- **A `git bundle` is probably the better production-grade version of
  this idea over raw `tar`**: same order-of-magnitude transfer cost (it's
  shipping the same already-compressed pack data), but self-contained,
  independently verifiable (`git bundle verify`), resumable/re-attemptable
  as a single file (vs. a directory tree that can be left half-written if
  the pipe dies), and it can be scoped to specific refs — e.g. bundle just
  the branches/PRs actually needed instead of every ref the client
  happens to have. Didn't benchmark it separately here since its payload
  is the same pack data `tar` already measured, but it's the one to
  implement for real rather than a literal `tar` pipe.
- Important asymmetry for question 4: **`git fetch --unshallow` always
  talks to the *original remote* (GitHub in this case), never to a
  sibling/peer** — so shallow-clone-then-unshallow by itself gets you fast
  *initial* availability but doesn't benefit from the fast client↔VM
  tunnel at all; the "inflate" step is still capped at the same ~25 MB/s
  GitHub saw in the reported baseline. If the client already holds a full
  clone, source the history backfill from the client (bundle/tar over the
  tunnel) instead of from GitHub, and use shallow-clone-from-GitHub only
  as the fallback when no such client copy exists yet.

### Recommended combined flow

1. **Fast path (client already has `core` cloned):** VM task does a
   metadata-only `git init` + remote add, then receives a `git bundle`
   (or raw `.git` tar) pushed from the client over the tunnel for the
   refs it needs; `git checkout` to materialize the working tree. This
   is the ~10-20s path end-to-end for the full history, not just current
   tip.
2. **Cold-start fallback (no client copy available / tunnel not up yet):**
   VM does `git clone --depth 1 --branch <ref>` directly from GitHub for
   immediate usability (bounded by current-tree size, not full history),
   then a background task does the full backfill — via the client bundle
   transfer above if/when it becomes available, falling back to
   `git fetch --unshallow` against GitHub (same ~25 MB/s cap, but
   non-blocking) if it never does.
3. Either way, once the VM's git metadata is aligned to the intended
   ref, hand off to Mutagen (`two-way-resolved`, alpha = client,
   `--ignore-vcs`) for ongoing working-tree sync of anything git doesn't
   already cover — uncommitted edits, untracked files, build output you
   want mirrored back, etc.
