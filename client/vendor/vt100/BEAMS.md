# Beams extension of vt100 0.18.1

Copied from the exact 0.18.1 source in the Cargo registry, retaining the MIT
license, upstream README and changelog. The manifest omits upstream-only test
dependencies; production dependencies/features are unchanged.

Changes expose read-only state snapshots, cell SGR formatting, DCS callbacks
and reset notifications. Saved attributes now belong to each buffer; xterm's
parameterless ANSI `CSI s`/`CSI u` aliases share the DEC slot. DEC restore clamps
an origin-relative saved row to the current margins, matching xterm's
`CursorRestoreFlags`. Beams' bounded stream adapter limits control sequences
before passing them to this crate; the upstream allocating `vte` parser remains
unchanged.

Behavioral regressions live in `client/src/terminal_model.rs` and the managed
PTY harness. When updating upstream, compare these changes and rerun those
checks. Vendoring does not establish fidelity for features upstream does not
implement, notably graphics and terminal resize/reflow.
Additional represented state includes ANSI insert mode, ASCII/DEC G0/G1
designations and SO/SI (saved per buffer), independent bold/faint and basic
blink/invisible/crossed-out SGR, and OSC 8 cell links/current link. Links use
shared immutable strings and are cleared from erased cells; native formatted
output retains their metadata. Client eligibility bounds link parameters and
rejects ambiguous cursor-save/link combinations. Cell layout is now larger
than upstream's 32-byte optimization.
The optional xterm.js 5.5 reference checks restoration bytes against that
emulator's unpainted baseline. Its saved-wrap/origin behavior differs from the
VT/xterm model; it does not establish complete shadow-model equivalence.

The inherited-initialization inventory adds reset-only controls for behavior
the backend already represents: ANSI/UTF-8 selection, enabled autowrap,
normal global video, disabled reverse wrap/horizontal margins/newline mode
unprotected characters and normal line size (`ESC # 5`). Unrepresented mouse-variant resets are admitted
conservatively; enabled variants remain unsupported. Beams records explicit
evidence rather than treating these parser defaults as observations. Initial
controls need an observed CAN/ST boundary, and each row needs normalization
before its cells/save state can be certified.
TBC 3 clears all tab stops per buffer, so HT then advances to the final column
and retains a pending wrap. Custom stops remain unsupported. After resize,
Beams requires a fresh clear before trusting previously cleared stops; this
fork does not model terminal-specific tab extension/reflow. Inherited REP
and ambiguous coupled mouse resets defer painting instead of assuming their
state is preserved by local output.
