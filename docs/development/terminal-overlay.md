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
