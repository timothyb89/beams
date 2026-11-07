// Independent replay of the Rust paint fixtures with @xterm/headless.
// See docs/development/terminal-overlay.md for the opt-in test command.
const assert = require('node:assert/strict');
const { Terminal } = require(process.argv[2]);

function write(terminal, bytes) {
  return new Promise(resolve => terminal.write(Uint8Array.from(bytes), resolve));
}

function snapshot(terminal) {
  const buffer = terminal.buffer.active;
  const lines = [];
  for (let y = 0; y < terminal.rows; y++) {
    const line = buffer.getLine(buffer.baseY + y);
    const cells = [];
    for (let x = 0; x < terminal.cols; x++) {
      const cell = line.getCell(x);
      // Pinned 5.5 internals: compare URI/id, not allocated numeric ids.
      const raw = terminal._core._bufferService.buffer.lines.get(buffer.baseY + y).loadCell(x,
        terminal._core._inputHandler._workCell);
      const link = cell.getWidth() === 0 || !raw.hasExtendedAttrs() ? null :
        terminal._core._oscLinkService.getLinkData(raw.extended.urlId) || null;
      cells.push([
        cell.getChars() || ' ', cell.getWidth(),
        cell.getFgColorMode(), cell.getFgColor(),
        cell.getBgColorMode(), cell.getBgColor(),
        cell.isBold(), cell.isDim(), cell.isItalic(), cell.isUnderline(), cell.isInverse(),
        cell.isBlink(), cell.isInvisible(), cell.isStrikethrough(),
        link,
      ]);
    }
    lines.push(cells);
  }
  return { lines, x: buffer.cursorX, y: buffer.cursorY, type: buffer.type,
    origin: terminal.modes.originMode };
}

async function main() {
  let input = '';
  for await (const chunk of process.stdin) input += chunk;
  const traces = JSON.parse(input);
  for (const trace of traces) {
    const options = { rows: trace.initialRows || trace.rows, cols: trace.initialCols || trace.cols,
      scrollback: 0, allowProposedApi: true };
    const expected = new Terminal(options);
    const actual = new Terminal(options);
    try {
      if (trace.beforeResize) {
        await write(expected, trace.beforeResize);
        await write(actual, trace.beforeResize);
        expected.resize(trace.cols, trace.rows);
        actual.resize(trace.cols, trace.rows);
      }
      if (trace.expectedModes) {
        await write(actual, trace.physical);
        for (const [name, value] of Object.entries(trace.expectedModes)) {
          assert.equal(actual.modes[name], value, `${trace.name}: ${name}`);
        }
        const pen = actual._core._inputHandler._curAttrData;
        assert.equal(!!pen.isBold(), trace.expectedPen.bold, `${trace.name}: bold`);
        assert.equal(pen.getFgColor(), trace.expectedPen.fg, `${trace.name}: foreground`);
        assert.equal(actual.buffer.active.type, trace.expectedBuffer, `${trace.name}: buffer`);
        assert.equal(actual._core.coreService.isCursorHidden, trace.expectedHidden, `${trace.name}: visibility`);
        continue;
      }
      await write(expected, [...trace.before, ...trace.hidden, ...trace.after]);
      await write(actual, trace.physical ||
        [...trace.before, ...trace.paint, ...trace.restore, ...trace.after]);
      assert.deepEqual(snapshot(actual), snapshot(expected), trace.name);
    } finally {
      expected.dispose();
      actual.dispose();
    }
  }
  console.log(`xterm.js paint reference passed (${traces.length} traces)`);
}
main().catch(error => {
  console.error(error);
  process.exitCode = 1;
});
