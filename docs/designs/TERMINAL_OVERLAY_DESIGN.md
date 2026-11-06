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
  screen. Keep requests pending under their existing expiry policy; suppressing
  a notification must never imply approval or rejection.
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

Integrate the chosen UX policy, including pending menu cancellation, preservation
of forms, and a useful way to discover when local UI is unavailable. Availability
feedback must itself avoid painting an unknown screen. Keep compatibility
reasons available in diagnostics without writing debug text into remote output.

Completion checks:

- Seed an inherited primary screen with sentinel content and a nonzero cursor;
  run a plain shell, request a menu/toast, then type and inspect scrollback.
- Exercise transitions between known and unknown buffers and unsupported
  sequences arriving mid-dialog; remote bytes and subsequent behavior survive.
- Oversized strings and unsupported graphics use the same tested eligibility
  policy. No fallback relies on an unconditional clear or guessed redraw.
- A pending approval remains pending/expirable when UI is unavailable; prefix
  forwarding and cancellation remain usable.

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

Start with milestone 1's model/backend spike and bounded framing contract,
using the saved-cursor and origin/margins failures as state round-trip examples.
Then land milestone 2 before broadening feature support. Milestone 3 shares
the same snapshots and can follow it without a second parser. Milestones 4
and 5 determine the supported envelope; milestone 6 validates it before
milestone 7 changes the default.
