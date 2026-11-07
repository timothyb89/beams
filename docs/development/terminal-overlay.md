# Terminal overlay readiness audit

Audit date: 2026-11-06; updated 2026-11-07. **Keep `--terminal-overlay` opt-in.** The recommended
tests exposed interactive bugs, now covered by regressions, and three original
release gates. All three gates are resolved. Passing the ordinary test suite is
not sufficient to enable this path by default.

The proposed [resolution plan](../designs/TERMINAL_OVERLAY_DESIGN.md) sequences
bounded parsing and state snapshots, reversible painting, query dispatch,
compatibility/lifecycle behavior, independent validation and the final default
switch. The model/backend foundation and represented cursor/margin painting
are implemented. Saved-cursor, origin/margins and DCS queries pass, and child
input is bounded without blocking the driver. Later compatibility/lifecycle
milestones remain pending.
The [next-session handoff](../designs/TERMINAL_OVERLAY_DESIGN.md#implementation-status-and-next-session-handoff)
records the current commits, code entry points and concrete next steps.

## Recommended test coverage

| TODO recommendation | Coverage and result |
| --- | --- |
| OSC title/clipboard | Model fixtures split BEL/ST commands at every byte boundary. The controlling-PTY harness checks title updates, clipboard writes/queries, reply routing, and one-time effects while the menu is open. Parser callbacks replace the menu-only OSC scanner, including commands that start before the menu opens. |
| Bracketed paste | Byte-at-a-time fixtures protect literal/enhanced menu prefixes inside remote paste, arbitrary escape text, and local pasted controls. The local menu enables bracketed paste and restores the remote setting. The PTY harness checks byte-exact remote paste. |
| Mouse encodings | Split-read fixtures cover X10, UTF-8/1005, SGR/1006 and urxvt reports. X10 payloads intentionally contain `q`/`y`; coordinates cannot close a menu or approve a request. Restoration disables obsolete mouse modes/encodings before applying the model's current state. |
| Alternate screens | Emulator fixtures cover entering, leaving and round trips while the menu is open, selecting the final buffer and removing the menu from the buffer it covered. An ordinary alternate-screen overlay preserves the workstation main screen that the shadow model never observed. Updates to inactive buffers across more complex transitions remain a gap. |
| Wide/combining text | Fixtures cover double-width cells, combining accents, Nerd Font private-use glyphs, RGB, inverse video with default colors, and pending right-margin wrap. Toast removal restores wide glyphs bisected by either edge. Actual font/emoji width differences still require visual testing. |
| Resize | The controlling-PTY harness resizes an open dialog through ordinary and small sizes, checks the child sees each size and verifies overlay close does not inject the takeover mode's one-column pulse. |
| Input/query replies | Model/PTY fixtures cover ordinary/private CPR, DSR, device queries, OSC clipboard replies and opaque DCS input replies. CPR, common DECRQM reports and complete DECRQSS SGR/margin requests use remote state at the request. Cancelled/oversized DCS never synthesize replies; unmodeled settings and hidden XTGETTCAP receive negative responses. Passthrough adds no model reply. Additional mode/advanced-keyboard probes remain gaps. |
| Sustained output/lifecycle | A bounded 32-chunk output channel and 16-chunk drain budget give input, resize, signals and daemon events turns during output bursts. User input and replies share a 256 KiB worker queue including in-flight bytes; only the worker blocks on child writes. The PTY harness checks a 4 MiB burst, 8192 replies with child reads paused, exact subsequent input order, menu close, output/resize/SIGTERM responsiveness, explicit queue-exhaustion errors, exit status and termios cleanup. The burst fixture now reads input during nonblocking output. |
| Chunk boundaries | The remote prefix is tracked across reads. Opening a menu cancels an unfinished sequence only in the physical terminal; closing replays the latest unfinished prefix before passthrough resumes. Fixtures cover CSI, OSC and split UTF-8. |

The ordinary emulator fixtures use `vt100` as the physical-terminal reference.
They exercise composition and restoration, but cannot establish fidelity for
features that `vt100` itself does not implement. The PTY harness exercises
the compiled client and real controlling-terminal behavior without a live
Beam, workstation daemon, SSH agent or package installation. An optional pinned
xterm.js reference also replays restoration fixtures and actual menu output
against an unpainted baseline.

## Reproducible blockers

Three acceptance tests in `client/src/terminal_model.rs` are executable release
gates. All three now pass unignored:

1. **Saved cursor/attributes:** `overlay_acceptance_saved_cursor`. An
   application can issue DECSC (`ESC 7`) while hidden, then DECRC (`ESC 8`)
   after the menu closes. The physical saved slot is stale because restoration
   only serializes the current cursor. The next character appears at a
   different location from the model. This affects later output, not just the
   overlay's appearance. **Resolved:** menu close restores per-buffer saved
   cursors and attributes; DEC and parameterless xterm-style ANSI aliases have
   follow-up printing and color fixtures.
2. **Origin and scroll margins:** `overlay_acceptance_origin_and_margins`.
   A remote application can leave a scroll region and origin mode enabled.
   Local absolute-position painting then addresses the wrong rows, and
   restoring formatted model content does not suspend/restore those modes.
   **Resolved:** the shared drawing transaction suspends and restores this
   state for menus and toasts, including toast removal outside the scroll
   region. CPR and the origin-mode report use remote state while local drawing
   uses absolute positioning.
3. **DCS queries:** `overlay_acceptance_dcs_query`. The model drops DECRQSS
   (`DCS $ q ... ST`) requests. A remote application that probes terminal
   settings while hidden can wait indefinitely or time out. A focused backend
   extension now exposes bounded DCS callbacks, but they are not yet
   dispatched as queries. **Resolved:** complete ST-terminated DECRQSS SGR and
   margin requests now receive remote-state replies before subsequent output
   changes state. CAN/SUB/aborting ESC and oversized strings never fabricate
   replies. Unmodeled settings and hidden XTGETTCAP receive negative replies;
   other DCS remains unsupported. Passthrough never adds a model reply.

Run all three gates in the ordinary suite:

```sh
cargo test --manifest-path client/Cargo.toml overlay_acceptance
```

Additional audit findings to resolve or explicitly handle with a compatibility
fallback before changing the default:

- The initial shadow screen starts blank at `(0,0)`; it cannot reconstruct
  workstation screen contents/cursor that existed before the PTY started.
  Zellij normally establishes a fresh alternate screen, but a plain remote
  shell does not necessarily do so. Repainting an unobserved primary screen,
  or an inactive screen updated while hidden, needs a deliberate recovery
  policy rather than an assumption that the model is complete.
- DEC character sets, insert mode, additional SGR attributes (such as strike
  and blink), OSC 8 hyperlinks, terminal graphics and advanced input-mode
  changes are incompletely modeled. OSC hyperlink state can also attach a
  remote link to local UI text. Suspend/restore those states or choose a
  compatibility path when the model cannot represent them.
- Parser-string retention is now bounded before the allocating parser by the
  shared 64 KiB framer. Hidden overflow closes the menu and resumes the exact
  original sequence rather than truncating it; further painting is suppressed
  until an observed full reset. Unit/PTY fixtures check opaque tails, reset
  recovery and that keys cannot approve through an unseen menu. This loss
  flag does not yet establish full content provenance or compatibility UX.
- Real terminal resize/reflow and emoji/grapheme widths can differ from the
  reference model. The current harness verifies PTY size propagation, not
  agreement with every emulator's reflow behavior.
- Saved-cursor semantics also vary. The xterm.js 5.5 reference drops pending
  wrap on restore and does not save origin mode, unlike the VT/xterm model.
  The optional replay checks transaction transparency on that emulator; it
  does not prove complete shadow-model agreement for hidden output using those
  features. Support the target terminal behavior or invalidate painting when
  that uncertainty matters.
- Cleanup covers ordinary terminal input modes. Kitty keyboard stacks,
  modifyOtherKeys, character-set and other application state still need
  independent lifecycle checks and restoration policy.

## Repeatable validation

```sh
cargo test --manifest-path client/Cargo.toml
cargo build --manifest-path client/Cargo.toml
./client/tests/managed-session-smoke.sh
BEAMS_TEST_OVERLAY=1 ./client/tests/managed-session-smoke.sh
python3 client/tests/terminal-overlay-smoke.py
```

Optional independent replay (Node and the separately installed module required):

```sh
npm install --prefix /tmp/beams-terminal-reference --ignore-scripts --no-audit --no-fund @xterm/headless@5.5.0
export BEAMS_TEST_XTERM_MODULE=/tmp/beams-terminal-reference/node_modules/@xterm/headless
cargo test --manifest-path client/Cargo.toml menu_restoration_survives -- --nocapture
python3 client/tests/terminal-overlay-smoke.py
```

`terminal-paint-reference.cjs` compares nine model-restoration traces and the
PTY harness's actual rendered menu output with the same remote output without
painting. It checks final cells/colors, cursor and origin mode after saved
cursor restore, printing and scrolling. It does not add a Node dependency to
the ordinary Rust suite. The cursor semantics above follow xterm's
`cursor.c::CursorRestoreFlags`; the ANSI aliases follow `charproc.c::CASE_ANSI_SC`
and `CASE_ANSI_RC`. The independent implementation is pinned because those
semantics are part of the test evidence.

The Python harness uses only the standard library and isolated fixture
directories; it kills fixture children and removes files on completion.
The existing smoke test also checks daemon-connected forms and path completion
in both display modes.

> **Known breakage (2026-11-07):** `main` at `94be084` removed
> `beams ssh --no-client-daemon` (every launch now needs the client daemon),
> but `managed-session-smoke.sh` and `terminal-overlay-smoke.py` still pass it,
> so both fail at argument parsing on `main` and on this branch. They passed on
> the same overlay code before the rebase. Fixing them needs a
> daemon-connected fake launch, which belongs to the launch workstream.

## Adding terminal recordings

Recordings of real sessions keep overlay eligibility honest: the gate must
become eligible for what supported multiplexers and terminals actually send,
and painting over that output must match an unpainted terminal. Zellij adapts
to reported terminal capabilities (it uses synchronized frames under iTerm2
and none without a terminal answering), so record each supported terminal.
Prioritize terminals that differ: one without synchronized output (e.g.
Terminal.app) and one with the Kitty keyboard protocol (Ghostty, kitty or
WezTerm).

Each recording has four parts. Keep raw files under `overlay-traces/` (git
ignored) until trimmed and reviewed.

1. **Terminal identity and size.** In the terminal under test, outside any
   multiplexer, and without resizing the window afterwards:

   ```sh
   mkdir -p overlay-traces/<terminal> && cd overlay-traces/<terminal>
   echo "$TERM_PROGRAM $TERM_PROGRAM_VERSION $TERM $(stty size)" > terminal.txt
   ```

2. **Entry-probe replies.** Strict passthrough sends no probes, so record the
   terminal's answers separately, outside beams, ssh, tmux and zellij, without
   typing while it runs:

   ```sh
   python3 <repo>/client/tests/terminal-probe-replies.py replies.bin
   ```

   It sends exactly `Baseline::probes()` (a unit test keeps the two in sync),
   saves the raw replies and reports when the last one arrived. Replies later
   than the client's 200 ms window are themselves a finding: that terminal
   gets no evidence for them.

3. **Session output in strict passthrough.** `--escape-key none` makes the
   client a pure pipe, so the file is exactly what the remote sent (plus the
   terminal's own echo). Use a scratch Beam and a harmless prompt: everything
   on screen is recorded.

   ```sh
   # macOS (BSD script): output only, keystrokes are not logged.
   script -q zellij-claude.trace beams ssh --purpose overlay-trace --escape-key none
   # Linux (util-linux script):
   script -q -c 'beams ssh --purpose overlay-trace --escape-key none' zellij-claude.trace
   ```

   Inside: let Zellij start, run `claude` in a scratch directory, type in its
   input box and let a reply stream for 10–20 seconds (frequent status/input
   redraws matter), then exit. For an attach redraw, reconnect with the same
   command in a second recording and exit after a few seconds. Do not resize:
   macOS `script` does not forward window-size changes; the PTY harness covers
   resize instead. Afterwards, `beams rm overlay-trace`.

4. **Line-size behavior.** In the terminal under test (outside multiplexers),
   both commands must print normal-width text; record the result in the
   workstation matrix. The overlay assumes this for `CSI 2J` and `?1049h`:

   ```sh
   printf '\e#6WIDE\e[2Jnormal?\n'
   printf '\e[?1049h\e#6WIDE\e[?1049l\e[?1049hnormal?'; sleep 2; printf '\e[?1049l'
   ```

Copy the files to a Beam checkout if recorded elsewhere, e.g. `beams copy-to
--purpose <purpose> zellij-claude.trace <checkout>/overlay-traces/<terminal>/zellij-claude.trace`.

### Trim, review and add

Trim to the multiplexer's lifetime: from its alternate-screen entry through
its exit line. Recordings made with clients before `9b51919` also end with the
client's own cleanup (starting `CAN ST ESC[?2026l`); drop it.

```sh
python3 -I - overlay-traces/<terminal>/zellij-claude.trace \
    client/tests/fixtures/zellij-<terminal>-<scenario>.bin <<'PY'
import sys
data = open(sys.argv[1], "rb").read()
cleanup = data.rfind(b"\x18\x1b\\\x1b[?2026l\x1b[?9l")
if cleanup != -1:
    data = data[:cleanup]
start = data.index(b"\x1b[?1049h")
exit_ = data.rindex(b"\x1b[?1049l")
end = data.find(b"\n", exit_) + 1 or len(data)
open(sys.argv[2], "wb").write(data[start:end])
print(len(data[start:end]), "bytes")
PY
```

Review the trimmed text before committing (paths, hostnames, Beam names,
tokens, prompt contents). Prefer a short attach redraw over a long session;
the committed reattach fixture is 23 KB. Copy `replies.bin` next to it as
`zellij-<terminal>-<scenario>.replies` when recorded.

Add an entry to `RECORDINGS` in `client/src/session/baseline.rs` with the
rows/cols from `terminal.txt`, and `replies: Some(include_bytes!(...))` when
recorded (otherwise `None`, which replays representative and no-DECRQSS reply
sets). Two tests then run for every recording:

- `recordings_are_paintable_from_observed_entry_state`: eligible after the
  first full frame (exactly, when synchronized frames are present), never
  lost before the multiplexer exits, unavailable afterwards, and never
  eligible without reported settings. Every recorded reply must be
  recognized; an unrecognized reply would reach the remote as input.
- `painting_over_recordings_matches_an_unpainted_terminal`: a toast at the
  first eligible boundary and a menu covering later output leave the terminal
  identical to an unpainted run, including an unseen workstation primary, in
  the model and (with `BEAMS_TEST_XTERM_MODULE`) in xterm.js.

When the eligibility test fails it prints the reason and the distinct frames
outside the supported inventory:

- `UnsupportedSequence`: classify each listed frame in
  `terminal_eligibility::supported`, with its protocol reference and why it
  cannot change represented cells or state. If it is a query or effect, make
  sure it is relayed once while hidden and that its reply routes to the remote
  (`is_terminal_reply`). Add a look-alike that must stay unsupported. Never
  accept a frame just because a recording contains it.
- `InheritedState` with no unsupported frames: entry replies or the redraw do
  not establish something (often a mode the terminal does not answer). Decide
  explicitly whether it can be observed, safely reset at entry, or must defer.

Record the terminal, versions, results and any new policy in the design doc
and this audit, alongside the fixture.

Recorded results in this Linux Beam environment: 188 client unit tests and
3 integration tests passed; the takeover smoke, overlay smoke and new overlay
protocol/PTY harness passed. The three acceptance gates were also run
explicitly and all failed with the cursor, row-offset and dropped-query
differences described above. They were ignored at that stage.
No independent graphical terminal or workstation font/reflow pass was
available here. Repository-wide formatting checks also report pre-existing
formatting differences; formatting changes were restricted to edited code.

After the model/backend foundation: 201 client unit tests and 3 integration
tests pass, along with both managed-session display modes and the expanded
protocol/PTY harness. The forward diagnostic test once missed its captured
stderr; its isolated and full-suite reruns passed, and TODO tracks the flake.
The three gates were rerun explicitly and still fail. The backend can expose
their needed state, but restoring it and answering DCS queries remain the next
work.

After shared cursor/margin painting: 206 client unit tests and 3 integration
tests pass, with one ignored DCS release gate. Both managed-session display
modes and the expanded protocol/PTY harness pass. Independent xterm.js replay
passes the nine restoration traces and actual origin/margin menu renderer
trace. Toast and takeover fixtures also preserve current/saved drawing state
through subsequent output. Insert mode, character sets, hyperlinks, complex
inactive-buffer updates and the semantics/provenance gaps above remain.

After merging the overlay work with the launch work on local `main`: 209 client
unit tests and 3 integration tests pass, with the DCS gate still ignored.
The four targeted bootstrap fetch tests, both managed-session display modes,
the overlay protocol/PTY harness and the launch bridge smoke test pass.
Independent replay again passes all nine restoration traces and the live menu
renderer trace.

After remote setting queries and bounded child-input writes (2026-11-07):
**215 client unit tests and 3 integration tests pass, with none ignored**.
The client build, both managed-session display modes and the expanded
protocol/PTY harness pass. The harness verifies split/cancelled DCS,
remote-state DECRQSS, negative replies and exactly-once passthrough, then
pauses child reads to exercise ordered reply/input delivery, local menu
close, output, resize and signals. An over-budget storm produces an explicit
error and restores termios. Three input-worker unit tests cover short writes,
in-flight accounting, asynchronous errors and teardown without joining a
blocked writer. Independent xterm.js replay passes the nine restoration
traces and actual renderer trace.

The old burst check twice missed its two-second deadline. Its fake SSH process
used a synchronous 4 MiB output write, preventing that fixture from reading
input during the burst. The fixture now multiplexes nonblocking output and
input; the corrected harness passes both with and without independent replay.
This fixes the fixture's fairness measurement. The separately tracked forward
diagnostic flake and real workstation validation remain unresolved.

For the final workstation pass, use the actual supported outer terminals with
Zellij, a plain shell, `less`, and an editor. Open/close the menu during a
redraw and while idle; paste multiline text containing the configured prefix;
exercise click/drag/scroll; resize both with a menu and without one; enter/exit
alternate-screen applications; inspect title/clipboard actions, colored
Unicode borders and emoji; then test normal exit, remote disconnect and local
termination. Check cursor placement, subsequent typing, scrollback and mouse
behavior after closing the menu and after the SSH process exits.

Enable the overlay by default only after the acceptance gates pass, unsupported
state has a tested compatibility policy, and this workstation pass succeeds.

Additional query-policy pass (2026-11-07): 217 unit tests and 3 integration
tests, client build and protocol/PTY smoke pass. DECRQM mouse/keypad/buffer
reports use remote state; unmodeled ANSI/DEC modes receive status 0. XTGETXRES
receives a negative response while hidden. Kitty flags, xterm modifier options
and version probes are physical, and split replies bypass menu actions.

Conservative eligibility/recovery pass (2026-11-07): 220 unit tests and 3
integration tests, client build, both managed-session display modes and the
protocol/PTY smoke pass. The PTY smoke also passes with the independent
xterm.js renderer comparison. Tests cover inherited-state refusal, rejection
before model mutation at every frame split, exact replay of unsupported
output, inactive-buffer round trips and reopening a preserved form. Only RIS
establishes a complete baseline today. Covered resize closes the menu and
invalidates eligibility, but restoring after the physical terminal has
already reflowed remains best effort and a release gate.

Common drawing-state pass (2026-11-07): 222 unit tests and 3 integration tests,
client build and the protocol/PTY harness pass. Twelve independent xterm.js
restoration traces pass, including insert mode, G0/G1 graphics and saved
mapping, independent SGR attributes and OSC 8 URI/id metadata; the actual
renderer comparison also passes. Toast tests cover restored hyperlinks and
unlinked local text, plus continued printing in insert/graphics mode.
Malformed color parameters no longer count as a complete SGR snapshot.

Observed lifecycle pass (2026-11-07): 227 unit tests and 3 integration tests,
client build, both managed-session modes and the protocol/PTY harness pass.
Independent replay passes twelve restoration traces, the actual renderer and
two live cleanup traces with non-default outer mouse, paste, keypad, insert,
origin/margins, SGR, buffer and cursor visibility. PTY checks also cover
retained startup input, keyboard-flag restoration without stack pops, cursor
style, normal exit, SIGTERM during a partial string and byte-exact disabled
prefix behavior with no probes or cleanup. A real-PTY unit test proves early
setup returns kill/reap the supervised child. Negative setting replies are
counted individually so one negative cannot finish three pending probes.

The shell fixture now waits for the first remote frame before typing, rather
than racing the bounded probe window. Native hyperlink serialization emits
controls only around linked text, keeping unlinked restoration compact.
Eligibility also rejects unsupported C0 controls, scrollback erasure, unknown
status probes, malformed multi-mode reports and ambiguous changed-character-set
restores before covered mutation. All original acceptance gates remain active.
Unknown initialization/saved state, original keyboard-stack history, useful
safe borrowing and physical resize/reflow remain unestablished; no workstation
matrix was available in this Beam. The overlay remains opt-in.

Post-resize eligibility pass (2026-11-07): **232 unit tests and 3 integration
tests pass, with none ignored**, along with the client build, both
managed-session smoke modes and the expanded protocol/PTY harness.
After resize of a known baseline, per-buffer evidence requires origin reset,
full margins, CUP, DECSC and ED 2; this reestablishes the active buffer without
another RIS. Missing components or writes at unknown coordinates defer
painting. Resize cannot weaken inherited/unsupported/budget loss into a
recoverable geometry reason. Covered buffer switches stop before mutation,
restore the safe buffer and replay the original frame once, including split
and leading-zero mode parameters. Target geometry requires initialization
again, and unknown inactive content is never serialized.

Pinned xterm.js 5.5 independently passes the twelve existing restoration
traces plus two new traces that actually resize before reinitialization,
check subsequent saved-state output, and return to the untouched primary.
Live replay passes the existing renderer/lifecycle traces and an additional
recovered-menu trace. The PTY fixture proves saved form input survives an
uncovered resize and incomplete initialization, then tests raw switching into
an unknown buffer and explicit recovery there. This establishes recovery
after a clear, not lossless covered reflow. Inherited initialization, regional
borrowing and the real workstation matrix remain open; takeover stays default.

Explicit inherited-initialization pass (2026-11-07): **241 unit tests and 3
integration tests pass, none ignored**. Shared confidence is recorded
independently from buffer selection, tabs and geometry. Complete supported
initialization can establish one buffer without ever observing RIS; the other
buffer stays unknown. Initializers cover attributes/link closure, character
sets, fixed supported drawing behavior, input settings and coupled mouse
groups. Controls before an observed CAN/ST boundary are insufficient, and
every row requires normal line-size evidence before cells or saves are known.
TBC 3 is modeled per buffer; unknown tabs lose cursor evidence, and
previously cleared stops require another clear after resize.

Eight model regressions check every split, omitted components, prior unknown
writes/save slots, stale partial mouse resets, explicit target recovery and
stronger unsupported/budget loss. A cleanup regression requires evidence for
the queried outer target buffer, preventing guessed inactive cursor/margins.
Independent xterm.js 5.5 replay adds three traces with non-default physical
state absent from the shadow: drawing, saved state, tabs and untouched primary
content compare against the same unpainted trace. The live inherited renderer
trace verifies remote CPR/DECRQSS, OSC 8 metadata and saved-state continuation.
The PTY fixture also verifies incomplete-init refusal, form persistence,
split/leading-zero switches replayed once and SIGTERM cleanup without any RIS.
The reference emulator does not exercise DEC double-width/height line effects.
Row normalization has model evidence regressions; its physical terminal effects
remain part of the workstation gate.

The client build, both managed-session smoke modes and expanded protocol/PTY
harness pass. The initialization inventory is deliberately restrictive:
ordinary clears/soft resets are insufficient, unrepresented enabled modes and
inherited REP remain unsupported, and ambiguous coupled mouse resets defer
painting. Regional evidence, safe borrowing, full model/REP semantics and the
real workstation font/reflow matrix remain open; the overlay stays opt-in.

Regional toast pass (2026-11-07): **246 unit tests and 3 integration tests
pass, none ignored**. Content confidence now records observed full-line
erases per buffer, separately from shared/current drawing state and the
saved-cursor/tab requirements of a full menu. Toasts can restore their rows
while leaving inherited content elsewhere and inherited save slots untouched.
Expanded wide-glyph edges and any outside glyph needed to restore pending
wrap must also be known. Storage is bounded by terminal height.

Three model regressions cover split initialization, missing erases/state,
confined edits, scroll/loss/recovery, pending wrap and promotion of all-row
erasure to complete content. Two toast regressions include independent
xterm.js 5.5 primary/alternate traces with unobserved sentinel cells and saved
cursors, wide glyphs on both edges, cell/current hyperlinks, origin/margins,
insert mode and character maps. Subsequent saved-cursor output matches an
unpainted session. Local toast bytes never select a buffer or save a cursor.
The earlier seventeen model traces and five live renderer/lifecycle traces
also pass, for nineteen model and five live independent comparisons.

The client build, takeover and overlay managed-session smoke tests and
expanded protocol/PTY harness pass. Regional evidence is deliberately limited
to full-width rows under known normal line sizes; possible scrolls or display
edits discard partial evidence instead of guessing movement of inherited
cells. Physical DEC line-size effects, finer regions, full model/grapheme/REP
semantics and covered reflow still need work. Knowing the primary buffer is
active does not establish ownership of an unknown inactive alternate buffer.
Borrowing remains gated pending ownership and independent switching/replay
tests, and the overlay remains opt-in.

Owned-scratch borrowing pass (2026-11-07): **254 unit tests and 3 integration
tests pass, none ignored**. An observed RIS with no later remote alternate use
establishes an unused scratch alternate; any 47/1049 selection, unsupported
output or budget loss revokes it, and resize keeps ownership while requiring
fresh primary geometry/save/tab evidence. Menus borrow it only when direct
painting is unavailable, at an output boundary, on the primary with origin off,
full margins and no pending wrap. Retention is bounded to 256 KiB of drawing
output; queries, effects, non-ASCII/unsupported frames, buffer switches and
exhaustion close and replay first. Close starts with CAN/ST and an explicit 47
selection, so cleanup after a partially written selector never clears the
primary.

Model, eligibility and `session/borrowed.rs` regressions cover ownership and
revocation, missing geometry, split/rejected frames, retention limits, partial
close writes and partial setup selectors. Two independent xterm.js traces and
the live PTY renderer (normal exit and SIGTERM while borrowing) match an
unpainted session for primary cells, saved cursors and the unused alternate,
with ordered query/effect replay, relinquishment and resize. Twenty-one model
and seven live independent comparisons pass. Both managed-session smokes and
the protocol/PTY harness pass. Ordinary inherited-shell borrowing without an
observed reset, covered reflow and the workstation matrix remain open.

Zellij eligibility pass (2026-11-07): **262 unit tests and 3 integration tests
pass, none ignored**. Strict-passthrough recordings of Zellij 0.45.1 with
claude under iTerm2 3.6.10 showed the gate never became eligible: Zellij sends
no RIS or explicit initialization, several unclassified controls, and on
reattach repaints every row without clearing. UTF-8 characters split across
reads were also misclassified (fixed separately in `24be6a9`).

Overlay sessions now seed evidence from entry probe replies and write one
set of unqueryable resets before remote output; Zellij's controls are
classified with hidden relays; saved slots and tab stops are lazy; full-row
overwrites establish rows; and `?1049h`/`CSI 2J` are assumed to reset line
sizes (to be verified per terminal). The committed reattach fixture is
eligible from the first read boundary after Zellij's first full frame until
Zellij exits. A toast and a covering menu over it match an unpainted terminal
in the model and xterm.js, including the unseen workstation primary.
Twenty-three model and seven live independent comparisons pass; both
managed-session smokes and the PTY harness pass. Real entry-probe replies,
other terminals' Zellij output, line-size behavior and the workstation matrix
remain open; the overlay stays opt-in.
