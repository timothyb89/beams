# Beams extension of vt100 0.18.1

Copied from the exact 0.18.1 source in the Cargo registry, retaining the MIT
license, upstream README and changelog. The manifest omits upstream-only test
dependencies; production dependencies/features are unchanged.

Keep changes focused on read-only state snapshots, DCS callbacks and reset
notifications. Beams' bounded stream adapter limits control sequences before
passing them to this crate; the upstream allocating `vte` parser remains
unchanged.

Behavioral regressions live in `client/src/terminal_model.rs` and the managed
PTY harness. When updating upstream, compare these changes and rerun those
checks. Vendoring does not establish fidelity for features upstream does not
implement, notably character sets, graphics and terminal resize/reflow.
