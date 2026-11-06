# Terminal overlay readiness audit

Audit date: 2026-11-06. **Keep `--terminal-overlay` opt-in.** The recommended
tests exposed interactive bugs, now covered by regressions, and three
reproducible default-readiness failures. Passing the ordinary test suite is
not sufficient to enable this path by default.

The proposed [resolution plan](../designs/TERMINAL_OVERLAY_DESIGN.md) sequences
bounded parsing and state snapshots, reversible painting, query dispatch,
compatibility/lifecycle behavior, independent validation and the final default
switch. The first model/backend foundation is now implemented; the three
restoration/query acceptance gates and later milestones remain pending.

## Recommended test coverage

| TODO recommendation | Coverage and result |
| --- | --- |
| OSC title/clipboard | Model fixtures split BEL/ST commands at every byte boundary. The controlling-PTY harness checks title updates, clipboard writes/queries, reply routing, and one-time effects while the menu is open. Parser callbacks replace the menu-only OSC scanner, including commands that start before the menu opens. |
| Bracketed paste | Byte-at-a-time fixtures protect literal/enhanced menu prefixes inside remote paste, arbitrary escape text, and local pasted controls. The local menu enables bracketed paste and restores the remote setting. The PTY harness checks byte-exact remote paste. |
| Mouse encodings | Split-read fixtures cover X10, UTF-8/1005, SGR/1006 and urxvt reports. X10 payloads intentionally contain `q`/`y`; coordinates cannot close a menu or approve a request. Restoration disables obsolete mouse modes/encodings before applying the model's current state. |
| Alternate screens | Emulator fixtures cover entering, leaving and round trips while the menu is open, selecting the final buffer and removing the menu from the buffer it covered. An ordinary alternate-screen overlay preserves the workstation main screen that the shadow model never observed. Updates to inactive buffers across more complex transitions remain a gap. |
| Wide/combining text | Fixtures cover double-width cells, combining accents, Nerd Font private-use glyphs, RGB, inverse video with default colors, and pending right-margin wrap. Toast removal restores wide glyphs bisected by either edge. Actual font/emoji width differences still require visual testing. |
| Resize | The controlling-PTY harness resizes an open dialog through ordinary and small sizes, checks the child sees each size and verifies overlay close does not inject the takeover mode's one-column pulse. |
| Input/query replies | Fixtures and the PTY harness cover ordinary/private CPR, DSR, device queries, OSC clipboard replies and opaque DCS input replies. CPR is answered from the remote model at the query's position, even if later bytes move its cursor; origin-relative CPR now uses exposed remote margins. Application-cursor, cursor-visibility and bracketed-paste DECRQM reports use remote state. Kitty prefix repeats/releases cannot toggle the menu. The PTY harness verifies 512 replies to 512 status queries without queue loss. Additional mode queries and DCS query answers remain gaps. |
| Sustained output/lifecycle | A bounded 32-chunk output channel and a 16-chunk drain budget give input, resize, signals and daemon events turns during output bursts. The PTY harness checks a 4 MiB burst, child exit status, SIGTERM, terminal termios restoration, and cleanup sequences for mouse/paste/alternate-screen modes. |
| Chunk boundaries | The remote prefix is tracked across reads. Opening a menu cancels an unfinished sequence only in the physical terminal; closing replays the latest unfinished prefix before passthrough resumes. Fixtures cover CSI, OSC and split UTF-8. |

The emulator fixtures use `vt100` as the physical-terminal reference. They
exercise composition and restoration, but cannot establish fidelity for
features that `vt100` itself does not implement. The PTY harness exercises
the compiled client and real controlling-terminal behavior without a live
Beam, workstation daemon, SSH agent or package installation.

## Reproducible blockers

Three ignored acceptance tests in `client/src/terminal_model.rs` are executable
release gates. They deliberately assert the required behavior and currently
fail; their ignore reasons link each failure to this remaining work:

1. **Saved cursor/attributes:** `overlay_acceptance_saved_cursor`. An
   application can issue DECSC (`ESC 7`) while hidden, then DECRC (`ESC 8`)
   after the menu closes. The physical saved slot is stale because restoration
   only serializes the current cursor. The next character appears at a
   different location from the model. This affects later output, not just the
   overlay's appearance.
2. **Origin and scroll margins:** `overlay_acceptance_origin_and_margins`.
   A remote application can leave a scroll region and origin mode enabled.
   Local absolute-position painting then addresses the wrong rows, and
   restoring formatted model content does not suspend/restore those modes.
   Origin-relative CPR is now covered, but painting/restoration still needs
   to suspend and restore this state.
3. **DCS queries:** `overlay_acceptance_dcs_query`. The model drops DECRQSS
   (`DCS $ q ... ST`) requests. A remote application that probes terminal
   settings while hidden can wait indefinitely or time out. A focused backend
   extension now exposes bounded DCS callbacks, but they are not yet
   dispatched as queries. Attribute/mode queries must use remote state where
   the dialog has temporarily changed physical state; forwarding alone is
   insufficient.

Run the gates separately so their failures are visible:

```sh
cargo test --manifest-path client/Cargo.toml overlay_acceptance -- --ignored
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

The Python harness uses only the standard library and isolated fixture
directories; it kills fixture children and removes files on completion.
The existing smoke test also checks daemon-connected forms and path completion
in both display modes.

Recorded results in this Linux Beam environment: 188 client unit tests and
3 integration tests passed; the takeover smoke, overlay smoke and new overlay
protocol/PTY harness passed. The three acceptance gates were also run
explicitly and all failed with the cursor, row-offset and dropped-query
differences described above. They remain ignored in the ordinary suite.
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
