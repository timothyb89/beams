# Terminal overlay default-readiness plan

Proposed 2026-11-06, following the
[readiness audit](../development/terminal-overlay.md). The overlay remains
opt-in while this plan is implemented. This document describes proposed work,
not additional guarantees in the current client.

## Outcome and scope

After a menu or toast disappears, both the visible screen and the behavior of
subsequent remote output must match a session without that local painting.
Preserving cells alone is insufficient: saved cursors, drawing modes, query
replies and buffer transitions affect what the application does next.

Keep ordinary remote output as byte-for-byte passthrough. Extend the shadow
model and use one controller for local painting, remote-state queries and
compatibility decisions. Apply the controller to menus **and toasts**, including
toasts in legacy takeover mode. Do not solve each finding with another
independent escape scanner or switch to rendering all remote output locally.

Default readiness means a tested supported set of terminal behavior and an
explicit policy outside it. It does not require implementing every terminal
graphics protocol. Do not advertise support for features solely because a
`vt100`-based reference test passes.

## Proposed compatibility policy

Prefer preserving the remote session when its screen cannot be restored:

- Do not paint a toast or open an overlay on an unobserved or unrepresentable
  screen. When the remote uses the primary buffer and the conditions below
  hold, borrow the alternate buffer for a manual menu without repainting the
  unknown primary content. Otherwise defer local painting.
- Keep requests pending under their existing expiry policy; suppressing a
  notification must never imply approval or rejection. Always log acceptance
  in the requesting command. Include the session's prefix when reliably known;
  additional terminal-state guidance is useful but optional if its transport
  requires substantial work.
- A menu request can wait for a recoverable state; a second prefix press still
  sends the original encoding remotely. Define cancellation and availability
  feedback before shipping so the prefix does not appear broken.
- Retain an explicit legacy takeover option for users who need that behavior.
  Its resize-driven redraw is best effort, particularly for plain shells;
  it must not be described as lossless recovery.
- If an unsupported sequence arrives while an overlay is already open, restore
  the last representable remote state and remove the local UI **before**
  forwarding that sequence. Preserve form contents and selection for reopening.
  If a transition cannot do this safely, it fails the default-readiness gate.

**Decided (2026-11-10): automatic takeover is the backstop.** A missing
prompt is a loss of basic function (approvals), and keys typed after an
unanswered prefix reached the remote. Whenever the overlay cannot paint, the
prefix opens a takeover menu, which clears the screen and pulses the remote
size on close. Its best-effort restoration is the accepted cost; the remote's
own repaint covers it under Zellij. Toasts that cannot be painted ring the bell
once. The "defer" wording above now applies only to painting *over* remote
content, never to whether a menu appears.

Further direction from the same review: require known *state* (cursor,
margins, modes, stream sync), but tolerate unknown *cells*. Restoring cells we
never saw as blank is within terminal norms (`clear`, inline pickers, takeover
itself). Push the pre-session screen into scrollback at entry so even that is
rare. All UI and input routing stays on the workstation: nothing may depend on
the Beam rendering prompts or sequencing its output.

### Detecting when restoration is unsafe

Track evidence in the output stream, rather than treating a Zellij process name
as a guarantee. Initially, inherited cells and cursor/mode state are unknown.
Observed initialization, clears and redraws can establish known regions and
state; observing an alternate-buffer selection alone cannot. Mark unsupported
changes, buffer transitions and uncertain resize behavior explicitly.

Each paint request asks whether the affected cells and required cursor/drawing
state can be restored. A toast may need less known content than a full menu.
This detects an unsafe precondition, not proof that physical screen corruption
has already occurred. A cursor report can establish cursor information but
cannot recover arbitrary inherited cells.

### Borrowing the alternate buffer for a plain-shell menu

Refinement from the follow-up discussion: unknown primary-buffer cells do not
necessarily make a manual menu impossible. If the remote is using the primary
buffer and Beams can safely own the inactive alternate buffer, leave the
primary contents untouched, show the menu in the alternate buffer, then return
and replay remote output. This can preserve inherited content without reading
or reconstructing it. It is a proposed additional display path, not behavior
in the current client.

Required conditions and behavior:

- Establish the physical and remote buffer ownership, and verify switching
  behavior in supported terminals. Do not assume an initial model default
  proves the physical terminal starts in the primary buffer.
- Preserve current and saved cursor state, drawing/input modes and any
  relevant inactive-buffer content. In particular, blindly using `1049`
  saves over a DECSC cursor slot. Buffer selection without implicit saving,
  together with explicit state preservation, needs its own acceptance tests.
- Drain and retain remote drawing output in a bounded buffer while the menu is
  open, keeping it off both the borrowed UI and the preserved primary screen.
  On close, restore the primary buffer/state before replaying those bytes.
  Close before a retention limit forces any bytes to be discarded.
- Continue answering queries and delivering terminal effects exactly once;
  replay must not duplicate an already answered query or clipboard action.
- If remote output starts an alternate-buffer transition, relinquish the
  borrowed buffer and restore/replay in order before forwarding that transition.
  Test resize/reflow and exit while borrowing as well.

There is no extra buffer to borrow when the remote already uses the alternate
buffer. Unknown or unsupported content there still requires deferred painting
or an explicitly chosen takeover. Likewise, borrowing fails eligibility when
cursor/mode state or inactive-buffer preservation cannot be established.
Use this fallback for manual menus; ordinary toasts must not switch the whole
screen just to display a notification.

### Inline approval guidance over the existing control pipe

Scope agreed in the follow-up discussion: always log the accepted request,
even when a toast succeeds. Extra information about an uncertain terminal state
is optional if expensive; it must not become a prerequisite for fixing overlay
restoration.

The first small change can use the existing `service_request_accepted` reply
and `fetch.rs` waiting message. Make acceptance a lasting stderr line rather
than a transient line overwritten by progress. Describe the request and where
to approve it; a generic workstation-menu instruction works without new
protocol messages. Include a key hint only when the configured key is reliably
known, handling custom and disabled prefixes. Do not hard-code the default or
assume environment inherited by a long-lived Zellij pane describes its current
attached SSH frontend.

For additional live availability information, use a separate advisory notice,
not a failed or decided request:

```text
SSH frontend -> workstation daemon -> client bridge
             -> Beam daemon -> requesting command's socket -> stderr
```

The frontend can report its session token, configured prefix, menu availability
(overlay, borrowed buffer, deferred, or disabled), notification availability,
and a bounded reason code. Correlate the requesting shell's session token with
the correct live frontend, including when several frontends share one Beam.
The bridge currently carries a request without that origin token, so extend
its advisory metadata as well as the frontend registration.

When that metadata is available, refine the acknowledgement. For example,
with the default prefix and a usable overlay or borrowed buffer:

```text
beams: request sent; press Ctrl-] in your Beams SSH session to review and approve (Ctrl-C to withdraw)
```

If reported availability says neither display path is usable, explain that
approval UI is unavailable and direct the user to a supported session or the
explicit takeover option. Do not instruct them to press a disabled or
ineffective prefix. Print through the requesting process's normal stderr so
the shell or Zellij renders it in place; Beams does not paint over unknown
cells to deliver this message.

Dynamic notices need additional work: `RequestStatus`
transitions reject `PendingApproval -> PendingApproval`, and `fetch.rs` ignores
pending-status messages. Add a capability-negotiated request-notice message
and structured availability data rather than weakening the approval state
machine for presentation updates. Render it in `beams fetch` and the other
waiting request CLIs, updating on meaningful availability changes without
repeating notices on every frame. Older peers retain existing wait messages.
Notices must not expose workstation paths or confer approval authority.

## Implementation sequence

Each milestone should be a separately reviewable change with its regressions,
README updates when behavior changes, and matching TODO updates.

### 1. Establish a bounded stream and complete state snapshots

Work primarily in `terminal_model.rs` and the remote-output controller in
`session.rs`; replace the overlapping boundary interpretation in
`session/toast.rs`.

Define a remote snapshot containing current and saved cursor state, attributes,
origin mode, scroll margins, wrap state, drawing/input modes and character-set
state. Record which state belongs to each screen buffer and which is global
according to the supported terminal semantics. Include content provenance and
the current unfinished sequence. Keep model updates and emitted effects ordered
at their position in the byte stream.

First make a small model/backend spike: expose the needed state and DCS events
through a focused extension or pinned fork of `vt100`/its parser. Prefer that
over a backend migration if it can meet the contract without duplicating the
emulator's cursor/mode semantics in another tracker. If it cannot, select a
backend with those capabilities before building restoration around it. Record
the dependency choice, maintenance cost and limits here.

Bound strings **before** they reach an allocating parser. Start with a 64 KiB
retention budget per control string, a byte budget as well as a count budget
for queued events, and the existing bounded PTY channel/drain scheduling.
Large clipboard/graphics payloads must be streamed transparently through a
non-compositing path rather than truncated or retained without a limit.
Budget exhaustion must trigger an ordered compatibility transition, preserve
original passthrough bytes, and retain only bounded framing state until the
real terminator/cancellation arrives. Do not treat one second of silence as
the end of an OSC/DCS, UTF-8 sequence or synchronized update.

Completion checks:

- Split OSC, DCS, CSI, UTF-8 and cancellation at every byte boundary; snapshots
  and effects agree regardless of read size.
- Multi-megabyte unterminated strings have bounded retained storage, input and
  resize remain responsive, and their payload cannot become local key actions
  or unintended screen text.
- Queue pressure never silently discards a query or required restoration
  effect. Test the explicit overflow policy and exactly-once delivery.
- The chosen model exposes every state item needed by milestones 2 and 3.

### 2. Make all local painting a reversible transaction

Introduce one painting transaction used by palette frames, toast paint/removal
and takeover rendering where applicable. It takes a remote snapshot, establishes
a predictable physical drawing environment, paints, then restores remote state
in a documented order.

Suspend origin mode, scroll margins, insert mode and character-set mapping
before absolute-position painting. Close remote OSC 8 hyperlinks around local
text and restore the remote link afterward. Restore cursor/attributes, modes,
margins and pending wrap without borrowing the application's saved-cursor slot.
When hidden output updates a saved slot, explicitly restore that updated slot
as well as the current cursor. Test DEC and ANSI save/restore operations
separately rather than assuming identical semantics.

Track every buffer touched while a menu is open, not only entry and exit
buffers. Restore only observed content; preserve the workstation primary screen
when the remote application owns a fresh alternate screen. Cover repeated
alternate-screen transitions, inactive-buffer mutations and resizes.

Completion checks:

- Remove the ignores from `overlay_acceptance_saved_cursor` and
  `overlay_acceptance_origin_and_margins`; both pass in the ordinary suite.
- Follow each restore with cursor restore, printable output, wrapping and
  scrolling operations. Matching a screenshot before those operations is
  insufficient.
- Test menu and toast paths with origin/margins, insert mode, DEC line drawing,
  hyperlinks, cursor visibility and wide glyphs at rectangle edges.
- A resize while covered does not silently claim fidelity when the chosen
  model's resize semantics disagree with the physical terminal.

### 3. Answer queries from the correct terminal state

Use the stream controller's ordered query dispatch instead of reconstructing
queries from incomplete callbacks.

Classify supported requests explicitly:

| Request | Source of the reply |
| --- | --- |
| CPR, including origin-relative/private forms | Remote cursor/margins at the query byte |
| Supported DECRQM modes | Remote mode state, including modes temporarily changed for local UI |
| DECRQSS for represented attributes and margins | Remote snapshot, encoded according to the request |
| Device capabilities and other physical properties unaffected by local UI | Physical terminal, with opaque input replies routed to the child |
| Unknown/unsupported requests | Protocol-defined unsupported reply where available, otherwise an explicit compatibility policy |

Add DCS framing and dispatch, with DECRQSS as the first acceptance target.
Inventory other probes used by the supported applications, including
XTGETTCAP and advanced keyboard queries. A physical query is safe to relay only
when local painting has not changed its answer. Do not classify mutating window
operations as read-only queries.

Completion checks:

- Remove the ignore from `overlay_acceptance_dcs_query`.
- PTY tests check exact replies, split queries/replies, mixed output and queries,
  and remote-state changes immediately after a query.
- Each supported query receives exactly one reply; passthrough must not add a
  model-generated reply alongside the physical terminal's reply.
- A terminal reply received during a menu never becomes a shortcut or form
  input, and query storms cannot overflow into silently dropped requests.

### 4. Enforce provenance and unsupported-feature policy

Use snapshot metadata to gate every local paint. The inherited primary screen
starts unknown; observing a cursor report alone does not reveal its cells.
An alternate-screen selection alone is also insufficient unless its contents
and relevant state have been established. Track confidence per buffer and
region, invalidating it for unsupported changes rather than painting stale
cells. Recover only through observed operations that actually establish the
required content and state.

Implement common state needed for supported daily applications: insert mode,
DEC character sets, missing SGR attributes and OSC 8 state. Inventory advanced
keyboard protocols, graphics, resize/reflow and grapheme-width behavior; either
support them or apply the declared compatibility policy. An ignored escape
sequence must not automatically mean "safe to composite."

Integrate the chosen UX policy, including the guarded borrowed-buffer path,
pending menu cancellation and preservation of forms. The request acknowledgement
is mandatory; richer inline availability notices are an optional separate
change. Availability feedback must itself avoid painting an unknown screen.
Keep compatibility reasons available in diagnostics without writing debug
text into remote output.

Completion checks:

- Seed an inherited primary screen with sentinel content and a nonzero cursor;
  run a plain shell, request a menu/toast, then type and inspect scrollback.
  Test both borrowed-buffer eligibility and refusal; verify hidden output,
  saved cursor slots, buffer changes and output-budget exhaustion survive.
- Exercise transitions between known and unknown buffers and unsupported
  sequences arriving mid-dialog; remote bytes and subsequent behavior survive.
- Oversized strings and unsupported graphics use the same tested eligibility
  policy. No fallback relies on an unconditional clear or guessed redraw.
- A pending approval remains pending/expirable when UI is unavailable; prefix
  forwarding and cancellation remain usable.
- A fetch logs request acceptance even without a toast; any key hint handles
  custom/disabled/unknown prefixes correctly.
- If dynamic notices are implemented, test multiple frontends,
  disconnect/reconnect, recovery, expiry and older peers. Sending a notice
  does not change approval status. These optional notices do not block the
  restoration work or default switch.

### 5. Restore only state owned by the client

Record a session-entry baseline where it can be observed or queried safely,
and distinguish remote changes from temporary local overrides. Balance local
mode changes; avoid generic resets or keyboard-stack pops that assume the
parent terminal started in its default state.

Audit Kitty keyboard push/pop, modifyOtherKeys, application cursor/keypad,
mouse/focus/paste, character sets, hyperlinks, synchronized updates and alternate
screens. Unsupported or unobservable baseline state needs a documented policy,
not an invented value. Resolve that policy against the supported terminals.

Completion checks: start with non-default outer-terminal state, then test normal
exit, remote disconnect, local SIGTERM/SIGHUP, output errors and termination
with a partial string/menu/toast active. Check termios and subsequent parent
shell input, cursor, paste, mouse and keyboard behavior. Existing exit-status
and signal propagation must remain intact.

### 6. Validate against an independent terminal and real workstations

Retain the current unit and controlling-PTY tests, and add a separate emulator
or real-terminal reference that does not use the same `vt100` implementation.
Select it by whether it implements the state being tested. Run identical
remote traces with and without menus/toasts and compare state **after subsequent
remote operations**, not just the local frame. Use generated combinations of
save/restore, margins, buffer changes, resize and queries with varied read splits.

Run the workstation matrix from the audit with the actual supported outer
terminals and versions recorded: Zellij, plain shell, `less`, an editor,
paste/mouse, resize/reflow, Unicode/emoji/fonts, title/clipboard, scrollback and
all exit paths. The graphical/font/reflow checks require a workstation; Beam
PTY tests cannot substitute for them. Record results and remaining support
limits in the audit and `client/README.md`.

### 7. Change the default in a separate final change

Only after milestones 1–6 pass:

- Run the ordinary client suite and all three smoke commands in the audit.
  All three acceptance tests must run unignored in the ordinary suite.
- Confirm independent and workstation results, bounded-memory behavior, and
  compatibility UX are recorded with no unresolved restoration failures in
  the supported set.
- Enable eligible overlay painting by default. Keep `--terminal-overlay`
  accepted for compatibility and add an explicit `--terminal-takeover` escape
  hatch; reject conflicting flags. Keep `--escape-key none` strict passthrough,
  including no toast painting or local query replies.
- Update CLI help, README and TODO in the same commit. Avoid combining the
  default switch with new terminal-state code.

## First implementation slice

The small durable `beams fetch` request acknowledgement is implemented using
the existing return path, with generic workstation-menu guidance. Progress
does not overwrite it. Session-specific hints remain optional future work.

The first model/backend slice is implemented with a focused, vendored `vt100`
0.18.1 extension (`client/vendor/vt100/BEAMS.md` records provenance and changes).
It exposes read-only current/saved cursor and attribute state, both buffers'
origin modes and margins, streaming DCS callbacks and reset notifications.
This demonstrates that the first two restoration gates can use the emulator's
state rather than another cursor/mode ledger. DCS semantics and unimplemented
drawing features still need work; vendoring does not confer support for them.

`client/src/terminal_stream.rs` is shared by the model and physical-output
tracker. It retains at most 64 KiB per unfinished control sequence and supplies
bounded frames to the allocating parser. On hidden-output
overflow, the driver closes the palette, restores the captured frame and
replays the retained original prefix before forwarding the chunk tail.
Remaining sequence bytes stay opaque; no clipboard payload is truncated or
partially published by the model. At the foundation commit, this recovery
still inherited the saved-cursor/origin restoration limitations addressed by
the following painting slice.

Budget exhaustion suppresses further overlay menus/toasts until an observed
full reset (`ESC c`). The input router keeps ordinary keys remote and supports
a doubled literal/enhanced prefix without interpreting unseen approval
shortcuts. This is the first conservative eligibility condition, not complete
content provenance or availability UX. The borrowed-buffer fallback is pending.

Event storage has count/byte budgets. The live relay drains events at each frame
so supported query/effect bursts do not hit those budgets; offline consumers
get an explicit overflow diagnostic. Origin-relative CPR now uses the exposed
remote margins. At this foundation stage, synchronous reply-write backpressure
was still unaudited; the 512-query PTY check did not prove a peer that stops
reading its input could not stall the driver. The later input slice below
resolves that gap.

Validation: the client suite passes (201 unit tests and 3 integration tests),
as do both managed-session display modes and the expanded controlling-PTY
checks for exact oversized-OSC recovery, unseen approval-key isolation,
full-reset recovery and 512 query replies.
At the foundation commit, all three explicit acceptance gates still failed
and remained ignored in the ordinary suite.

This foundation supplies the snapshots used by the following painting slice.

## Saved cursor and margin painting slice

`client/src/terminal_paint.rs` now owns the shared physical drawing setup and
represented-state restoration. Menus, model restoration and toast paint/removal
use full vertical margins with origin mode off. Close restores each covered
buffer's saved cursor and attributes before its current margins, origin,
cursor, attributes and visibility. Pending wrap is recreated by reprinting the
existing final glyph, including wide glyphs. A toast restores only current
state, leaving the application's save slot untouched.

Saved attributes are now per-buffer in the vendored emulator; xterm-style ANSI
save/restore aliases are modeled explicitly. Saved origin-relative rows clamp
to the current margins on restore, following xterm's `CursorRestoreFlags`.
Takeover captures entry state so deferred original bytes replay from that
state rather than the model after it has consumed them.

The saved-cursor and origin/margins gates are unignored and pass. Additional
fixtures exercise subsequent printing/scrolling, saved attributes, narrow/wide
saved wrap, distinct buffer slots and toast placement outside remote margins.
An opt-in `@xterm/headless` 5.5 comparison verifies nine restoration traces and
actual controlling-PTY renderer output against an unpainted baseline.

That comparison does not establish complete model fidelity. The independent
emulator restores saved wrap/origin differently from the VT/xterm model; hidden
output that relies on those differences still needs terminal-specific support
or conservative invalidation. Insert mode, character sets, OSC 8 links, more
complex inactive-buffer mutations and resize/reflow provenance also remain.
Resolving these remaining gaps is necessary to finish milestone 2 alongside
the compatibility work; the overlay stays opt-in. The independent
graphical/workstation matrix remains pending.

Milestone 3's DECRQSS slice below now uses these snapshots. Retain the remaining
painting and compatibility items above as explicit gates before the default switch.

## Remote setting query slice

Implemented 2026-11-07. The shared framer holds a string's ST together, and
separates an aborting ESC command. DCS unhook alone is no longer treated as
completion: CAN/SUB, an aborting escape and budget exhaustion never synthesize
a reply. Complete bounded DECRQSS `m` and `r` requests receive SGR and vertical
margin replies from the remote snapshot at ST, before subsequent output.

Unmodeled DECRQSS settings receive `DCS 0 $ r ST`. SGR also receives that negative
response after an unmodeled attribute until an observed full reset; a partial
attribute snapshot is not advertised as complete. Hidden XTGETTCAP receives
`DCS 0 + r ST`, because its dynamically adjusted key capabilities may depend on
temporary local input modes. Other DCS commands remain unsupported while
hidden. With the menu closed, original requests reach the physical terminal
and the model injects no replies. Incoming physical DCS replies remain opaque
to local menu shortcuts.

The protocol encoding follows xterm's official control-sequence specification,
under "Request Status String (DECRQSS)" and "Request Termcap/Terminfo String
(XTGETTCAP)". This is a limited setting-query implementation, not a complete
terminal-probe inventory or a general unsupported-feature policy.

Validation: all 212 client unit tests and 3 integration tests pass, with all
three acceptance gates unignored. The client build, both managed-session modes
and expanded controlling-PTY harness pass. Fixtures cover every request split,
cancellation, overflow, immediate state changes, unsupported responses,
split physical DCS replies and exactly-once passthrough. At this slice, the
existing burst responsiveness assertion missed its two-second deadline once
and passed on rerun; the later input slice corrects its synchronous fixture
write. Painting bytes did not change; workstation gates remain pending.

## Observed entry state and Zellij eligibility (2026-11-07)

Dogfooding over iTerm2 3.6.10 showed the conservative gate made the overlay
unusable under Zellij 0.45, the main supported workflow. Two strict-passthrough
recordings (startup and reattach, `client/tests/fixtures/zellij-iterm2-reattach.bin`
holds the reattach redraw) showed why: Zellij never sends RIS or the explicit
initialization, uses several unclassified controls, and on reattach never
clears; it relies on `?1049h` and repaints every row. A read split inside a
UTF-8 character was also misclassified as unsupported; text frames now carry
whole characters across reads (`24be6a9`).

Decisions (agreed with the user):

- **Classify Zellij's controls explicitly.** `?12`, `?1004`, `?2031`, `?1015`
  (h/l), Kitty `CSI > N u` / `CSI < N u`, `SGR 59`, `CSI ? 996 n` and the
  exact bounded `DCS 26661 n <base64> ST` are accepted; a Kitty graphics
  `a=q` query is accepted but other graphics commands are not. While hidden,
  the theme query, graphics query and Zellij DCS are relayed once; replies and
  focus reports route to the remote. Recordings from other terminals may add
  entries: Zellij adapts to reported capabilities (it used 316 synchronized
  frames under iTerm2 and none without a terminal answering).
- **Observe what a terminal can report; reset only what none can.** The entry
  probes now include `?5`, `?7`, `?45`, `?69` and ANSI `20`. The model processes
  a synthesized initialization equal to the replies (unrecognized modes count
  as off; unrepresentable or unanswered values stay unknown; ANSI mode is
  proved by the replies). Overlay sessions then write `ENTRY_RESETS` once,
  before any remote output: `CAN ST`, `ESC % G`, `ESC ( B`, `ESC ) B`, `SI`,
  `CSI 0 " q` and an OSC 8 close. This is the single exception to "observe,
  never inject": no terminal can report these, and they equal an ordinary
  shell's state. Takeover and `--escape-key none` send none of it.
- **Lazy saved slot and tab stops.** Painting uses neither. A menu no longer
  requires them; covered `DECRC` of an unknown slot, `DECSC` from uncertain
  state, or a tab at unknown stops closes the menu and replays raw, and
  restoration never writes an unknown saved slot.
- **Buffer switches carry default origin/margins.** When the departing buffer's
  values are known defaults and the model reports defaults for both buffers,
  global (xterm) and per-buffer semantics agree.
- **Line sizes are assumed normal after `?1049h` entry or `CSI 2J`** (xterm's
  `ClearBufRows` resets DEC double-size lines). Line size cannot be queried and
  every painted row depends on it, so it cannot be lazy. This is an explicit
  assumption to verify per terminal in the workstation matrix, e.g.
  `printf '\e#6WIDE\e[2Jnormal?\n'` and
  `printf '\e[?1049h\e#6WIDE\e[?1049l\e[?1049hnormal?'; sleep 2; printf '\e[?1049l'`.
- **A complete row overwrite establishes that row.** Contiguous text from
  column one to the right edge, with only non-moving, non-mutating controls in
  between, writes every cell. Cursor movement, erases, tabs, backspace, a wide
  glyph that cannot fit or writing past the edge end the run. The in-row text
  bound now uses the backend's own `unicode-width` widths; widths above two
  remain conservative.

Validation: 262 client unit tests and 3 integration tests pass with none
ignored; build, both managed-session smokes and the PTY harness (whose fake
terminal now answers every probe) pass. The reattach fixture becomes eligible
at the first read boundary after Zellij's first full frame, stays eligible
until Zellij leaves the alternate screen, and is never eligible without
reported settings. A toast after the first frame and a menu covering the
second leave the terminal identical to an unpainted one, including the
unseen workstation primary, in both the model and xterm.js. The uncommitted
startup recording behaves the same at 64-byte, 512-byte and 4 KiB reads.
Independent replay passes 23 model traces and 7 live traces.

Limits: the iTerm2 replies to the entry probes have not been recorded (strict
passthrough sends none), so fixtures use representative replies; a terminal
without DECRQM gets no entry evidence. The line-size assumption, real
terminal widths and other terminals' Zellij output remain workstation gates.
The overlay stays opt-in.

## Implementation status and next-session handoff

Row-level content evidence is implemented (2026-11-07). Separate the cells and
state required by a toast from the full menu gate. After complete shared
initialization and selected-buffer normal-line/current cursor/origin/margin
evidence, `CSI 2K` proves one full-width row. Toasts require their restoration
rows, including wide-glyph edges, and any outside row whose last glyph must be
reprinted to restore pending wrap. They never use or overwrite an inherited
saved cursor, tab stops or inactive buffer. Other rows can remain inherited.
The observer retains only a terminal-height-bounded row map, not another cell
or cursor emulator.

Confined writes and row edits preserve row confidence. Possible wrapping,
scrolling, display edits, unknown coordinates, resize and buffer transitions
discard partial evidence rather than guessing how unknown cells moved. Fresh
full-line erases recover it. Once all rows are known, promote to complete
content so supported hidden scrolling keeps known source cells; menus still
require saved-slot and tab evidence. Normal line sizes remain a whole-buffer
precondition; finer cell/line-size regions and partial scroll propagation are
future work. Three model tests and two toast tests include two independent
primary/alternate replay traces with inherited sentinel content and saved slots.

Borrowing remains gated. A baseline report that the primary buffer is active
does not identify the inactive alternate contents, saved slot or ownership.
Regional painting never switches buffers and does not supply that missing
evidence. The next borrowing slice must establish a genuinely owned scratch
buffer, preserve its relevant contents/state, and independently test selection
and ordered bounded replay before enabling a fallback. Keep ordinary inherited
primary sessions deferred when those preconditions are absent.

Owned-scratch borrowing is implemented (2026-11-07): establish an unused
scratch alternate only from an observed RIS, and revoke it on any remote alternate selection or
unsupported/budget loss. Preserve that ownership through resize while requiring
fresh primary geometry/save/tab evidence. Use the fallback only on the primary,
with origin off, full margins and no pending wrap; never infer ownership from
entry probes or explicit primary initialization alone. Prefer direct overlay
when complete content is available.

Select 47 without implicit saving, paint only in the unused alternate, and
restore its blank default state before returning to the primary. Restore entry
current state without touching primary cells or save slots, then replay retained
remote drawing bytes exactly once. Bound retention to 256 KiB. For this first
path, close before queries, terminal effects, non-ASCII output, buffer changes,
unsupported controls or retention exhaustion rather than synthesizing answers
from uncertain cells. Their original bytes run after earlier retained drawing,
so the real primary terminal supplies replies/effects once. Preserve forms for
explicit reopening. Store the entry snapshot and retained bytes with terminal
cleanup so error paths can relinquish the borrowed buffer too.

Close output begins with CAN/ST and an explicit 47 selection, so cleanup after
a partially written selector clears only the scratch buffer, never the primary.
Borrowing is offered only at an output boundary and only when direct painting
is unavailable. 1047/1048 remain outside the supported inventory, so they
close and replay like any unsupported frame. Model tests cover ownership,
revocation by every alternate selection, resize recovery and missing geometry
components; `session/borrowed.rs` tests cover retention limits, split and
rejected frames, partial close writes and partial setup selectors. Two
independent xterm.js traces and the live PTY renderer (normal exit and SIGTERM
while borrowing) verify primary cells, saved cursors, the unused alternate,
ordered query/effect replay, buffer relinquishment and resize. This narrowly
owned path does not establish ordinary inherited-shell borrowing, covered
reflow fidelity or default readiness.

Explicit inherited initialization is implemented (2026-11-07): observe a supported
initialization of the state used by painting instead of requiring RIS.
Track shared attribute/character-set/input-mode evidence separately from
selected-buffer geometry, ownership, line size and tabs. SGR reset does not prove OSC 8
closure or character protection; mode-group resets must cover unrepresented
mouse variants too. Require CAN/ST before accepting initialization, ANSI/UTF-8
selection, supported wrap/linefeed/margin behavior, explicit tab clearing and
normal line size on every row, coordinates, a new save and ED 2.
Writes or saves before the relevant state is known cannot be certified
retroactively. Keep inactive content unknown and close/replay all buffer
transitions in this path. Unsupported output and sequence loss still need RIS;
a soft reset, ordinary clear or application name is not complete evidence.
Do not broaden REP into this path: its inherited previous graphic character
and local repaint effects need separate representation. Verify from a
non-default physical baseline that is absent from the shadow model. Eight
model tests and three independent traces cover missing components, read
splits, primary-content preservation, unknown tab/cursor/save state, stronger
loss reasons and coupled mouse groups. The live PTY renderer checks
initialization, queries, hyperlinks, form persistence and exactly-once buffer
switch replay without any RIS. Cleanup now requires evidence for the observed
outer target buffer; a known active buffer cannot authorize repositioning an
unknown inactive one.

The required initialization inventory is bounded to the supported ANSI/UTF-8
terminal behavior; it is not a claim about every terminal protocol. The
client observes these controls and never injects initialization:

| State | Evidence |
| --- | --- |
| Parser/grammar/encoding | CAN followed by ST, then `ESC <`, `ESC % G`; controls before the boundary are insufficient |
| Represented drawing behavior | `CSI ?7h`, `CSI ?5l`, `CSI ?45l`, `CSI ?69l`, `CSI 20l`, `CSI 0"q` |
| Character maps | G0/G1 ASCII or DEC graphics designations, SI or SO |
| Attributes/link | SGR reset and an explicit OSC 8 close |
| Drawing/input modes | ANSI insert/replace, application cursor, keypad, cursor visibility and bracketed-paste settings |
| Synchronized output | Observed `CSI ?2026l` |
| Mouse tracking/encoding | Complete resets of 9/1000/1001/1002/1003 and 1005/1006/1015/1016 |
| Selected buffer | Explicit 47 selection; a first selection invalidates earlier buffer-local evidence |
| Buffer state | TBC 3, DECOM reset, full margins, CUP, a fresh DECSC and ED 2 |
| Line size | CUP to column one and `ESC # 5` for every row, in order, before saving/clearing |

Superseded in part by the Zellij eligibility section: overlay sessions also
derive shared state from entry probe replies plus `ENTRY_RESETS`; TBC and a
fresh DECSC are no longer required to paint (dependent hidden output closes
instead); `?1049h` entry or `CSI 2J` establishes normal line sizes; and a
complete row overwrite establishes that row's cells.

Shared and normal-line evidence must be complete before saving/clearing;
printing before modes and
coordinates agree invalidates earlier clear/cursor evidence. Unknown DECRC or
a 1049 return can restore inherited attributes/maps/modes, requiring new
shared initialization. Partial mouse resets cannot be reused after an enable;
ambiguous resets of a different active protocol defer painting. Reset-only
controls do not authorize unsupported enabled modes. Previously cleared tabs
need another clear after resize. Row-level evidence is implemented above;
finer regions, borrowing and workstation font/reflow validation remain gates.

Post-resize eligibility is implemented (2026-11-07): distinguish loss of screen
geometry from loss of inherited/protocol state. Resize after a known RIS baseline preserves
shared mode/attribute evidence, but invalidates cells, cursor, margins, origin
and saved-slot geometry independently in both buffers. Explicit DECOM reset,
full margins, CUP, DECSC and ED 2 can reinitialize one buffer without another
RIS. Drawing at an unknown position must invalidate any earlier clear evidence.
Do not infer this initialization from a TUI name, buffer selection or a clear.
After this recovery, covered buffer switches stop before mutation and replay
raw after restoring the last safe state. The target needs initialization
again: emulator buffer switches can transfer cursors and clear alternate
content differently. Restoration leaves unknown inactive content untouched.
Initial inherited state also has the explicit initialization path above.
Unsupported output and sequence-budget loss still require observed RIS.
Five unit tests cover missing components, unknown-position writes, stronger
loss reasons and split/leading-zero buffer switches. Independent xterm.js
checks include two actual resize traces and the live recovered menu.
This geometry slice does not establish regional borrowing or lossless covered reflow.

Observed entry/exit ownership is implemented (2026-11-07). Before forwarding
child output, a 200 ms / 8192-byte capture queries recognized ANSI/DEC modes,
SGR, margins/cursor style, Kitty current flags and xterm modifier options.
Ordinary input waits for the normal router; expected probe replies are
consumed only in this bounded window. Cleanup restores observed values,
disables mouse modes before enabling the observed protocol and selects
buffers without clearing or saving. Coupled unknown mouse state is preserved.
Origin/margins require an observed final cursor and buffer ownership; an
unchanged DECOM is not written because it homes the cursor. Known settings
have normal-exit and SIGTERM/partial-string PTY checks and independent mode/
attribute/buffer/visibility comparison. Strict disabled-prefix sessions send
no entry probes, local responses or exit cleanup bytes; only termios is owned.

This is not complete lifecycle fidelity. Unanswered settings remain unknown;
a killed remote may leave them unbalanced. Current Kitty flags can be
restored, but the original stack history is unqueryable and no unowned stack
is popped. Saved slots/character-set baselines and real resize/reflow remain
unestablished. Early setup returns now kill/reap the spawned child through a
guard, with a real-PTY regression.
Keep these gates open and keep the overlay opt-in.

Common drawing-state coverage is implemented (2026-11-07): ANSI insert mode,
ASCII/DEC G0/G1 designations and SO/SI, per-buffer saved character sets, basic
blink/invisible/crossed-out SGR and independent bold/faint, and OSC 8 cell
links/current link. Menus/toasts suspend these while drawing and restore them.
Native cell serialization retains link targets, and toast removal uses raw
remote cells rather than losing links in a style-only UI buffer. Link parameters
and URI lengths are bounded. Malformed colors, richer underline attributes,
other charsets and active-link cursor saves/restores defer painting. Twelve
independent xterm.js traces now compare basic SGR and URI/id metadata as well
as cells, cursor and origin. These additions do not establish reflow fidelity.
Cursor restores whose saved character sets differ from the current sets defer
painting as well; the reference emulator's saved-map semantics differ from
the VT model's saved designations.

Conservative repaint eligibility and frame recovery are implemented (2026-11-07).
Inherited state starts unknown in both buffers; cursor reports, alternate
selection and clears alone cannot establish inherited modes/save slots.
An observed RIS (`ESC c`) or complete explicit initialization establishes
shared state initially. This is intentionally restrictive; regional evidence
remains future work, not permission to assume an ordinary TUI initialized
every mode. Unsupported controls invalidate eligibility until another RIS.
Resize loses per-buffer geometry; explicit recovery is described above.
Reasons distinguish inherited state, unsupported output, resize and budgets.

The shared framer now returns a rejected frame's complete original bytes,
including earlier-read prefixes. A covered unsupported frame stops before
model mutation. The driver restores the last representable screen, closes the
menu and resumes those bytes exactly once. Resets also close/replay physically.
Form contents and selection survive. Toasts are removed from the last good
state before incoming raw output can invalidate it. Menus track every buffer
touched while hidden, including inactive-buffer updates in round trips.

Deferred availability uses a bell after a safe output boundary; it never
draws over unknown content or terminates an unfinished OSC with BEL. Ordinary
keys cancel the pending prefix and remain remote input; doubled prefixes
retain literal forwarding. Recovery requires an explicit retry and reopens
the preserved form. Borrowing remains refused when inactive-buffer ownership
or saved state is unknown. Post-resize initialization establishes only the
selected buffer and does not authorize borrowing an inactive one. A fallback that extends
availability still requires richer evidence and independently verified buffer
switching, so this gate remains open rather than assuming a spare buffer.

Covered resize remains a fidelity gate: the outer terminal may already have
reflowed before the client sees SIGWINCH. Closing from the last snapshot is
best effort; new painting is suppressed until explicit geometry recovery or RIS.
The menu itself reopens immediately as takeover, so keys after the resize
never reach the remote.
This does not establish lossless
resize/reflow support and must not enable the default switch. Safe borrowing
and terminal-specific resize validation remain separate work.

The additional query-policy slice is implemented (2026-11-07). DEC mode reports
cover application cursor/keypad, origin, visibility, alternate buffers, modeled
mouse tracking/encoding and paste. Unmodeled ANSI/DEC modes receive DECRQM
status 0 (not recognized), rather than waiting for a reply. XTGETXRES receives
its negative resource response while hidden. Physical Kitty flags, xterm key
modifier options and terminal version probes are relayed because local drawing
does not change those settings; split replies remain opaque to menu actions.
Window operations 11–21 are reports; resizing/title-stack mutations are effects
or unsupported operations, never read-only queries.

The supported inventory is deliberately explicit. Other DCS/CSI requests with
no defined negative reply need the eligibility/recovery path below; they must
not be guessed from a local UI cursor or silently declared compositable.
Protocol references: xterm's control-sequence specification (DECRQM,
XTQMODKEYS, XTVERSION, XTGETXRES) and Kitty's keyboard-protocol specification
("Progressive enhancement"). Client validation: 217 unit tests and 3 integration
tests, build and protocol/PTY smoke pass; all acceptance gates remain unignored.

As of 2026-11-07, represented-state painting, remote setting queries and bounded
child-input writes are implemented. **Keep the overlay opt-in; takeover remains
the default.** All three original release gates pass unignored, including
`overlay_acceptance_dcs_query`. Compatibility and content provenance remain
incomplete; passing these three fixtures does not establish default readiness.

Completed work is preserved in these commits. Those up to `28eb7a8` are on
`main`; the rest are on the `terminal-overlay-readiness` branch:

| Commit | Result |
| --- | --- |
| `7e86344`, `318f309` | Initial input/relay hardening, expanded acceptance coverage and readiness audit |
| `92ba010`, `db5d68c`, `f574a4e` | Resolution plan, guarded fallback policy and separation of mandatory logging from optional notices |
| `e6dee81` | Durable `beams fetch` acknowledgement with generic workstation-menu guidance |
| `cb5cc47` | Bounded stream framing, model snapshots, streaming DCS callbacks and conservative overflow recovery |
| `28eb7a8` | Shared saved/current cursor, SGR, origin/margin restoration; toast and takeover fixes; independent replay fixtures |
| `33424d3` | ST-aware remote-state DECRQSS replies, negative replies for unmodeled settings/XTGETTCAP and unignored DCS gate |
| `1b9c006`, `cc98a25` | Child-input backpressure worker; the supported query policy |
| `0a45b94`, `1111022`, `223fba8` | Deferred unsafe painting with recovery; insert/charset/SGR/link state; bell acknowledgement |
| `9b51919` | Observed outer-terminal baseline and cleanup; child ownership on setup errors |
| `45ed059`, `5712b93`, `f9d1eec` | Post-resize and explicit inherited initialization; regional toast evidence |
| `fefca31` | Borrowed RIS-owned scratch alternate for menus over uncertain primaries |
| `24be6a9` | Whole UTF-8 characters across read splits |
| `71566c4` | Zellij eligibility: classified controls, entry evidence and resets, lazy save/tabs, row overwrites |

**Branch state for resuming (2026-11-07).** Work happens on the local branch
`terminal-overlay-readiness` (worktree `~/.codex/worktrees/851a/beams`),
rebased onto `main` at `94be084`; it is not merged into `main` and not pushed,
by choice. `terminal-overlay-readiness-pre-rebase` keeps the pre-rebase
history and can be deleted once the rebased branch is accepted. `main` keeps
moving in another worktree (`~/beams`): rebase again before merging. Raw
recordings live in the ignored `overlay-traces/`; only trimmed fixtures are
committed.

Start by reading `AGENTS.md`, this section, `TODO.md` and `git log main..`,
and check `git status` before changing anything. Push or merge only when
requested. Do not depend on previous conversation history or `/tmp` files
surviving (recreate the xterm.js module from the audit's instructions).

Implementation entry points:

| File | Responsibility |
| --- | --- |
| `client/src/terminal_stream.rs` | Shared framing, partial sequences, synchronized-output boundaries and the 64 KiB sequence budget |
| `client/src/terminal_model.rs` | Remote shadow state, ordered events and remote-state CPR/DECRQSS replies |
| `client/src/terminal_eligibility.rs` | Conservative evidence, availability reasons and the supported control inventory |
| `client/src/terminal_paint.rs` | Shared local drawing setup and represented-state restoration |
| `client/src/session.rs` | `drain_output`, hidden-output dispatch, raw replay, menu snapshots and child-input writes |
| `client/src/session/input.rs` | Ordered 256 KiB input/reply queue and blocking-write worker, with asynchronous error reporting |
| `client/src/session/baseline.rs`, `client/src/session/child.rs` | Bounded entry probes, observed-state cleanup and child ownership on setup errors |
| `client/src/session/toast.rs` | Physical-output tracker and toast paint/removal |
| `client/vendor/vt100/BEAMS.md`, `client/vendor/vt100/src/state.rs` | Vendored changes, provenance and read-only drawing snapshots |
| `client/tests/terminal-overlay-smoke.py`, `client/tests/terminal-paint-reference.cjs` | Live controlling-PTY coverage and optional independent replay |
| `client/src/session/borrowed.rs` | Borrowed scratch-alternate menus: bounded retention, ordered close/replay |
| `client/tests/fixtures/`, `RECORDINGS` in `baseline.rs` | Trimmed real-terminal recordings and the tests run over each |
| `client/tests/terminal-probe-replies.py` | Records a terminal's answers to the entry probes |
| `bootstrap/src/fetch.rs` | Persistent request acknowledgement; no new notice protocol |

Done (2026-11-10, usability reset): smokes repaired through a fake profile
launch, with a no-RIS variant; per-menu takeover fallback; borrowing opt-in
(`BEAMS_OVERLAY_BORROW=1`); probes only in overlay sessions, ended by a DA1
sentinel, late replies claimed for 5 s, one per probe, and `1$rr` accepted
as full margins; a bell for unpaintable toasts. See the audit's
"Usability reset" entry.

Next work, in separately reviewable changes:

1. **Gate simplification.** At entry, query CPR, then send linefeeds at the
   bottom row to push the pre-session screen into scrollback; this behaves the
   same in every terminal, unlike `CSI 2J`. Then send the client's own
   initialization (the inventory below, restoring 8-column tab stops after TBC 3,
   plus a DECSC at home). Require known state, tolerate unknown cells, use the
   resize pulse when restoration is doubtful, and keep takeover for lost
   stream sync or unsupported output. Overlay-only; takeover sessions are
   unchanged.
2. **Toast flicker** over frequently redrawn rows (claude's input/status
   lines under Zellij). Today a toast is erased before each output chunk and
   repainted after 30 ms of quiet output. Erase, pass through and repaint in
   one synchronized update when a chunk ends at a boundary (not inside a
   remote synchronized block or partial sequence); otherwise keep the
   delayed repaint. Zellij emits its own synchronized frames under iTerm2.
3. **Terminal coverage.** Add recordings per
   [Adding terminal recordings](../development/terminal-overlay.md#adding-terminal-recordings):
   iTerm2's real entry-probe replies, a terminal without synchronized output
   and one with the Kitty keyboard protocol, plus the line-size checks.
4. **Diagnostics (optional).** An opt-in file recording entry replies and
   availability reasons would make workstation results self-explanatory.
5. Extend inactive-buffer ownership beyond the RIS-owned scratch path and
   settle covered resize fidelity; then the workstation matrix and, last and
   separately, the default switch.

Update READMEs, audit and TODO with each slice; rerun independent replay when
painting changes.

Child-input backpressure is resolved by one ordered worker queue for user input
and query replies. Its 256 KiB limit includes in-flight writes. Only the worker
can block in an OS write; the session driver never waits for queue capacity or
joins a blocked writer. A full queue produces an explicit session error and
restores the terminal rather than dropping input or replies. Asynchronous write
errors reach the driver. The controlling-PTY fixture can now stop reading while
continuing to emit output: it proves menu-close, output, resize and SIGTERM
responsiveness, exact reply/user-input order after reads resume, and cleanup
when a query storm exhausts the budget. The normal burst fixture now uses
nonblocking output so it reads input during the 4 MiB burst, correcting a
fixture stall that made a fairness assertion depend on output throughput.
This failure policy bounds memory; it does not promise continued service for
a peer that never drains its input.

Current validation (rebased onto `main` at `94be084`): **266 client unit
tests and 3 integration tests pass, with zero ignored tests**, and the client
builds (one dead-code warning from `main`'s daemon-flag removal). The
managed-session smokes and PTY harness fail on `main` and here for the
reason in step 1. Before the rebase the same overlay code also passed the
takeover and overlay managed-session smokes and the expanded protocol/PTY
harness. Pinned xterm.js 5.5 replay again
passes twelve restoration traces, two resize traces, three inherited-state
traces, two regional toast traces, two borrowed-menu traces, five live
renderer traces (including owned borrowing in its normal-exit and SIGTERM
runs) and two live non-default lifecycle traces.
The harness also covers explicit initialization without any RIS, deferred recovery and
strict disabled-prefix partial-string termination. No real
workstation font/reflow or graphical-terminal validation has been recorded.

The earlier painting slice's recorded validation was **206 passing client unit tests, one ignored
DCS gate, and 3 passing integration tests**. The client build, takeover smoke,
overlay smoke and protocol/PTY harness passed. Optional xterm.js 5.5 replay
passed nine restoration traces and one actual menu-renderer trace; fetch's four
targeted bootstrap tests passed in its acknowledgement slice. The audit
contains the commands and historical results. One forward-diagnostics test
missed captured stderr once and passed its isolated/full reruns; TODO tracks
that unresolved flake.

Validation of the earlier combined `main` merge passed: **209 client unit tests and
3 integration tests**, with the DCS gate still ignored, plus the four targeted
bootstrap fetch tests. Both managed-session display modes, the overlay
protocol/PTY harness and the launch bridge smoke test passed. Optional xterm.js
replay again passed all nine restoration traces and the live renderer trace.

The independent replay compares painting with the same unpainted remote trace;
it does **not** prove full model agreement. xterm.js 5.5 saved-wrap/origin
semantics differ from the model. Recreate the pinned optional npm module using
the audit's instructions rather than relying on this session's `/tmp` install.
No real workstation font/reflow or graphical-terminal pass has been recorded.

Retain these decisions and remaining gates:

- Never paint over content the model cannot restore, but always show a menu:
  takeover is the automatic backstop (decided 2026-11-10). Unknown inherited
  primary cells cannot be recovered from a cursor report; push them into
  scrollback at entry rather than refusing to paint.
  Zellij is eligible from observed entry state and its own full redraw, not
  from its name; a session that bypasses it gets no such evidence.
- A borrowed alternate buffer is implemented only for a RIS-owned, unused
  scratch alternate. Ordinary inherited ownership remains unestablished; never
  nest over a remote-owned alternate screen or use it for ordinary toasts.
- Durable inline request logging is implemented in all accepted-fetch cases.
  Session-specific prefix/uncertainty notices remain optional, skippable work;
  generic menu guidance uses the existing return path today.
- Common insert/character-set/SGR/link state and every touched inactive buffer
  are represented. Extend initialization/region evidence and resize/reflow
  provenance. Saved-wrap/origin/changed-character-set variants conservatively
  invalidate painting; richer support needs agreement with the target terminal.
- Finish unqueryable entry/exit state and remote keyboard-stack ownership.
  Observed settings and early child cleanup are implemented. Complete
  independent model validation and the recorded workstation matrix with Zellij,
  a plain shell, pager/editor, resize, scrollback, fonts and lifecycle checks.
- Enable the overlay by default only after those gates pass, in a separate
  change preserving explicit takeover and strict `--escape-key none` behavior.
