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

This is the recommended UX choice, subject to user preference. Automatically
falling back to legacy takeover would preserve menu availability, but would
retain the audit's unknown-screen restoration risk. Decide this policy before
integrating automatic eligibility checks; passing the three existing
acceptance tests alone does not settle it.

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
partially published by the model. This recovery still inherits the existing
saved-cursor/origin restoration limitations until milestone 2 lands.

Budget exhaustion suppresses further overlay menus/toasts until an observed
full reset (`ESC c`). The input router keeps ordinary keys remote and supports
a doubled literal/enhanced prefix without interpreting unseen approval
shortcuts. This is the first conservative eligibility condition, not complete
content provenance or availability UX. The borrowed-buffer fallback is pending.

Event storage has count/byte budgets. The live relay drains events at each frame
so supported query/effect bursts do not hit those budgets; offline consumers
get an explicit overflow diagnostic. Origin-relative CPR now uses the exposed
remote margins. Query-phase work must also audit synchronous reply-write
backpressure; the current 512-query PTY check does not prove a peer that stops
reading its input cannot stall the driver.

Validation: the client suite passes (201 unit tests and 3 integration tests),
as do both managed-session display modes and the expanded controlling-PTY
checks for exact oversized-OSC recovery, unseen approval-key isolation,
full-reset recovery and 512 query replies.
All three explicit acceptance gates still fail and remain ignored in the
ordinary suite.

Next: milestone 2's shared painting transaction, starting with the saved-cursor
and origin/margins gates. Milestone 3 shares those snapshots. Milestones 4 and
5 complete the supported envelope, and independent/workstation validation
still precedes the default switch. Optional richer request notices can follow
once availability information actually exists.
