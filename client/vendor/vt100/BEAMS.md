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
implement, notably character sets, graphics and terminal resize/reflow.
The optional xterm.js 5.5 reference checks restoration bytes against that
emulator's unpainted baseline. Its saved-wrap/origin behavior differs from the
VT/xterm model; it does not establish complete shadow-model equivalence.
