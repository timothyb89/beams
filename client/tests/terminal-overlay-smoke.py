#!/usr/bin/env python3
"""Hermetic controlling-PTY checks for the overlay, without a real Beam.

Build first with cargo build --manifest-path client/Cargo.toml, then run this
file. The fake tsh logs terminal input and accepts output via fixture files;
tests synchronize on observations rather than fixed typing delays. This is
a protocol/PTY test, not a replacement for visual checks in real emulators.
"""

import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import shlex
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time
import tty


def fake_tsh(args):
    if args[:2] == ["beams", "ls"]:
        print(json.dumps([{
            "id": "managed-session-test",
            "uuid": "11111111-2222-3333-4444-555555555555",
            "owner": "test@example.com",
            "expires": "2099-01-01T00:00:00Z",
        }]))
        return 0
    if not args or args[0] != "ssh":
        raise RuntimeError(f"unexpected fake tsh arguments: {args!r}")
    root = Path(os.environ["BEAMS_OVERLAY_FIXTURE"])
    tty.setraw(0)
    (root / "remote-pid").write_text(str(os.getpid()))

    def size_changed(*_):
        rows, cols, _, _ = struct.unpack("HHHH", fcntl.ioctl(0, termios.TIOCGWINSZ, b"\0" * 8))
        with (root / "sizes").open("a") as log:
            log.write(f"{rows} {cols}\n")

    signal.signal(signal.SIGWINCH, size_changed)
    size_changed()
    os.write(1, b"\x1b[?1049h\x1b[?1002h\x1b[?1006h\x1b[?2004hREMOTE READY")
    (root / "ready").touch()
    command = 1
    with (root / "input").open("ab", buffering=0) as log:
        while True:
            path = root / f"command-{command}"
            if path.exists():
                payload = path.read_bytes()
                if payload == b"EXIT":
                    # Ordinary programs leave their terminal modes on exit.
                    os.write(1, b"\x1b[?1002l\x1b[?1006l\x1b[?2004l\x1b[?1049l")
                    return 17
                os.write(1, payload)
                (root / "ack").write_text(str(command))
                command += 1
            if select.select([0], [], [], 0.01)[0]:
                data = os.read(0, 65536)
                if not data:
                    return 0
                log.write(data)


class Session:
    def __init__(self, root, binary):
        self.root = root
        self.command = 0
        self.output = bytearray()
        self.status = None
        config = root / "config" / "beams-client"
        config.mkdir(parents=True)
        fixture = Path(__file__).parent / "fixtures/config/beams-client/state.json"
        (config / "state.json").write_bytes(fixture.read_bytes())
        bindir = root / "bin"
        bindir.mkdir()
        fake = bindir / "tsh"
        fake.write_text(f"#!/bin/sh\nexec {shlex.quote(sys.executable)} "
                        f"{shlex.quote(str(Path(__file__).resolve()))} --fake-tsh \"$@\"\n")
        fake.chmod(0o755)
        self.pid, self.master = pty.fork()
        if self.pid == 0:
            try:
                (root / "tty").write_text(os.ttyname(0))
                fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
                (root / "before-termios").write_text(
                    json.dumps(termios.tcgetattr(0), default=list))
                env = dict(os.environ)
                env.pop("SSH_AUTH_SOCK", None)
                env.update(
                    PATH=f"{bindir}:{env['PATH']}",
                    XDG_CONFIG_HOME=str(root / "config"),
                    XDG_RUNTIME_DIR=str(root / "runtime"),
                    BEAMS_OVERLAY_FIXTURE=str(root),
                    TERM="xterm-256color",
                )
                os.execve(binary, [binary, "ssh", "--terminal-overlay", "--no-client-daemon",
                                   "--purpose", "managed-session-test"], env)
            except BaseException:
                os._exit(99)
        try:
            self.wait(lambda: (root / "ready").exists(), "remote startup")
            self.slave = os.open((root / "tty").read_text(), os.O_RDWR | os.O_NOCTTY)
            self.wait(lambda: b"REMOTE READY" in self.output, "initial remote frame")
        except BaseException:
            self.close()
            raise

    def pump(self):
        if select.select([self.master], [], [], 0.01)[0]:
            try:
                self.output.extend(os.read(self.master, 65536))
            except OSError as exc:
                if exc.errno != errno.EIO:
                    raise
        if self.status is None:
            pid, status = os.waitpid(self.pid, os.WNOHANG)
            if pid:
                self.status = os.waitstatus_to_exitcode(status)

    def wait(self, predicate, description, timeout=5):
        deadline = time.monotonic() + timeout
        while not predicate():
            self.pump()
            if time.monotonic() >= deadline:
                raise AssertionError(f"timed out: {description}; exit={self.status}")
        self.pump()

    def send(self, data):
        os.write(self.master, data)

    def remote_input(self):
        path = self.root / "input"
        return path.read_bytes() if path.exists() else b""

    def emit(self, data):
        self.command += 1
        tmp = self.root / "pending-command"
        tmp.write_bytes(data)
        tmp.rename(self.root / f"command-{self.command}")
        self.wait(lambda: (self.root / "ack").exists()
                  and (self.root / "ack").read_text() == str(self.command), "remote output command")

    def open(self):
        offset = len(self.output)
        self.send(b"\x1d")
        self.wait(lambda: b"Beams local menu" in self.output[offset:], "menu opening", timeout=2)

    def close(self):
        for pid_file in ["remote-pid"]:
            try:
                os.kill(int((self.root / pid_file).read_text()), signal.SIGKILL)
            except (FileNotFoundError, ProcessLookupError):
                pass
        if self.status is None:
            try:
                os.kill(self.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            os.waitpid(self.pid, 0)
        if hasattr(self, "slave"):
            os.close(self.slave)
        os.close(self.master)

    def assert_termios_restored(self):
        before = json.loads((self.root / "before-termios").read_text())
        # Character slots are bytes, so compare via the same JSON conversion
        # used for the fixture's initial state.
        after = json.loads(json.dumps(termios.tcgetattr(self.slave), default=list))
        assert after == before, "terminal raw/echo modes leaked on session exit"


def run_checks(root, binary):
    session = Session(root, binary)
    try:
        # Neither a literal nor an enhanced prefix inside paste opens a menu.
        paste = b"\x1b[200~a\x1db\x1b[93;5u\x1b]pasted text\x1b[201~"
        session.send(paste)
        session.wait(lambda: session.remote_input() == paste, "byte-exact remote paste")
        session.emit(b"\x1b]2;BOUNDARY TITLE")
        session.open()
        session.emit(b"\x1b\\")
        session.wait(lambda: b"\x1b]2;BOUNDARY TITLE\x1b\\" in session.output,
                     "OSC started before menu relayed once")
        baseline = session.remote_input()
        # X10 coordinates deliberately contain q/y: they must not close or
        # approve anything in the menu. The following reports stay local too.
        session.send(b"\x1b[M qy\x1b[<0;12;9M\x1b[32;12;9M")
        session.emit(b"\x1b[3;7H\x1b[6n\x1b[?6n\x1b[5n\x1b[?2004l\x1b[?2004$p\x1b[>0c")
        replies = b"\x1b[3;7R\x1b[?3;7;1R\x1b[0n\x1b[?2004;2$y"
        session.wait(lambda: session.remote_input() == baseline + replies, "remote cursor/status replies")
        session.wait(lambda: b"\x1b[>0c" in session.output, "device query relay")
        device_reply = b"\x1b[>1;2c"
        session.send(device_reply)
        session.wait(lambda: session.remote_input() == baseline + replies + device_reply,
                     "physical device reply routing")
        storm_baseline = session.remote_input()
        session.emit(b"\x1b[5n" * 512)
        session.wait(lambda: session.remote_input() == storm_baseline + b"\x1b[0n" * 512,
                     "query burst answered without queue loss")

        # Emit an OSC split across commands, including its ST terminator.
        session.emit(b"\x1b]2;OVERLAY TITLE")
        session.emit(b"\x1b\\\x1b]52;c;SGVsbG8=\x07\x1b]52;c;?\x07")
        session.wait(lambda: b"\x1b]2;OVERLAY TITLE\x1b\\" in session.output, "title relay")
        session.wait(lambda: b"\x1b]52;c;SGVsbG8=\x1b\\" in session.output, "clipboard write relay")
        session.wait(lambda: b"\x1b]52;c;?\x1b\\" in session.output, "clipboard query relay")
        clipboard_reply = b"\x1b]52;c;cXk=\x1b\\"
        session.send(clipboard_reply)
        session.wait(lambda: session.remote_input().endswith(clipboard_reply), "clipboard reply routing")

        # Resize several times while the dialog owns the terminal.
        for rows, cols in [(30, 90), (8, 32), (24, 100)]:
            fcntl.ioctl(session.master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
            session.wait(lambda: f"{rows} {cols}\n" in (root / "sizes").read_text(), "PTY resize")
        session.emit("\x1b[H界e\u0301 \x1b[7mdefault inverse\x1b[0m".encode())
        # A partial remote CSI must be replayed before passthrough resumes.
        session.emit(b"\x1b[38;2;")
        session.send(b"q")
        session.wait(lambda: session.output.endswith(b"\x1b[38;2;"), "unfinished CSI restored")
        session.emit(b"1;2;3mAFTER CLOSE\x1b[0m")
        session.wait(lambda: b"1;2;3mAFTER CLOSE" in session.output, "remote continuation")
        session.send(b"REMOTE INPUT")
        session.wait(lambda: session.remote_input().endswith(b"REMOTE INPUT"), "input after menu close")
        assert "24 99\n" not in (root / "sizes").read_text(), "overlay injected a resize pulse"
        assert session.output.count(b"\x1b]52;c;SGVsbG8=\x1b\\") == 1, "clipboard effect duplicated"

        # Exercise the live menu under origin mode and restricted margins.
        # Saved attributes change while hidden, then SC/RC and scrolling
        # must work after close. Optionally replay the actual renderer bytes
        # against the same remote trace in an independent xterm.js emulator.
        offset = len(session.output)
        before = (b"\x1bc\x1b[?1049h\x1b[2;23r\x1b[?6h\x1b[2;3H"
                  b"\x1b[31mORIGIN BEFORE\x1b7\x1b[?25l")
        session.emit(before)
        session.wait(lambda: b"ORIGIN BEFORE" in session.output[offset:], "origin fixture startup")
        session.open()
        hidden = b"\x1b[4;9H\x1b[32m\x1b7\x1b[1;1HORIGIN HIDDEN\x1b[6n\x1b[?6$p"
        query_baseline = session.remote_input()
        session.emit(hidden)
        session.wait(lambda: session.remote_input() == query_baseline + b"\x1b[1;14R\x1b[?6;1$y",
                     "origin-relative query while menu owns the physical cursor")
        session.send(b"qORIGIN INPUT")
        session.wait(lambda: session.remote_input().endswith(b"ORIGIN INPUT"), "origin menu close")
        after = b"\x1b8Z\r\nAFTER ORIGIN"
        session.emit(after)
        session.wait(lambda: b"AFTER ORIGIN" in session.output[offset:], "scrolling after origin menu")
        if module := os.environ.get("BEAMS_TEST_XTERM_MODULE"):
            trace = dict(name="live origin menu", rows=24, cols=100,
                         before=list(before), hidden=list(hidden), after=list(after),
                         physical=list(session.output[offset:]))
            subprocess.run(["node", str(Path(__file__).with_name("terminal-paint-reference.cjs")), module],
                           input=json.dumps([trace]), text=True, check=True)
        session.emit(b"\x1bc\x1b[?1049hORIGIN CHECK FINISHED")
        session.wait(lambda: b"ORIGIN CHECK FINISHED" in session.output, "origin fixture cleanup")

        # Exceed the model's string budget while hidden. Recovery must close
        # the menu and resume the exact original bytes, including the prefix
        # retained across earlier reads, without partially publishing the OSC.
        session.open()
        offset = len(session.output)
        oversized = b"\x1b]52;c;" + b"A" * (64 * 1024 + 19)
        session.emit(oversized)
        session.wait(lambda: oversized in session.output[offset:], "oversized OSC prefix recovery")
        blocked_offset = len(session.output)
        session.send(b"\x1dyAFTER LIMIT")
        session.wait(lambda: session.remote_input().endswith(b"yAFTER LIMIT"),
                     "unsafe model cannot interpret unseen approval keys")
        ending = b"\x1b\\AFTER OVERSIZED"
        session.emit(ending)
        session.wait(lambda: oversized + ending in session.output[offset:],
                     "oversized OSC resumed byte-for-byte")
        assert session.output.count(oversized + ending) == 1, "oversized OSC duplicated"
        assert b"Beams local menu" not in session.output[blocked_offset:], "unsafe menu reopened"
        session.emit(b"\x1bc\x1b[?1049hRECOVERED MODEL")
        session.wait(lambda: b"RECOVERED MODEL" in session.output, "full model reset")

        # Sustained output must yield to local input. The fixture writes a
        # burst larger than both client queues while the menu is open.
        session.open()
        session.command += 1
        (root / f"command-{session.command}").write_bytes(b"x" * (4 * 1024 * 1024))
        session.send(b"qAFTER BURST")
        session.wait(lambda: session.remote_input().endswith(b"AFTER BURST"),
                     "input responsiveness during output burst", timeout=2)
        session.wait(lambda: (root / "ack").read_text() == str(session.command), "burst drained")
        session.command += 1
        (root / f"command-{session.command}").write_bytes(b"EXIT")
        session.wait(lambda: session.status is not None, "child exit")
        assert session.status == 17, f"wrong child status: {session.status}"
        session.assert_termios_restored()
    finally:
        session.close()


def main():
    client = Path(__file__).resolve().parent.parent
    binary = str(client / "target/debug/beams")
    with tempfile.TemporaryDirectory(prefix="beams-overlay-smoke-") as scratch:
        root = Path(scratch)
        normal = root / "normal"
        normal.mkdir()
        run_checks(normal, binary)
        interrupted = root / "interrupted"
        interrupted.mkdir()
        session = Session(interrupted, binary)
        try:
            session.open()
            os.kill(session.pid, signal.SIGTERM)
            session.wait(lambda: session.status is not None, "handled SIGTERM")
            assert session.status == 143, f"wrong SIGTERM status: {session.status}"
            session.assert_termios_restored()
            for mode in [b"\x1b[?1002l", b"\x1b[?1006l", b"\x1b[?2004l", b"\x1b[?1049l"]:
                assert mode in session.output[-512:], f"mode cleanup missing on SIGTERM: {mode!r}"
        finally:
            session.close()
    print("terminal-overlay protocol/PTY smoke test passed")


if __name__ == "__main__":
    if sys.argv[1:2] == ["--fake-tsh"]:
        sys.exit(fake_tsh(sys.argv[2:]))
    main()
