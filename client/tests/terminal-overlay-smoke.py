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
    # Profile launches create a Beam, copy the runtime and launch to it, then
    # open the daemon's bridge alongside the interactive session.
    if args[:2] == ["beams", "add"]:
        print(json.dumps({
            "id": "managed-session-test",
            "uuid": "11111111-2222-3333-4444-555555555555",
            "owner": "test@example.com",
            "expires": "2099-01-01T00:00:00Z",
        }))
        return 0
    if args[:2] in (["beams", "exec"], ["beams", "scp"]):
        return 0
    if any("client-bridge" in arg for arg in args):
        # No Beam daemon here: keep the bridge quiet until the test ends.
        time.sleep(30)
        return 1
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
    if os.environ.get("BEAMS_OVERLAY_INITIALIZED", "1") == "1":
        os.write(1, b"\x1bc\x1b[?1049h\x1b[?1002h\x1b[?1006h\x1b[?2004hREMOTE READY")
    else:
        os.write(1, b"PLAIN REMOTE READY")
    (root / "ready").touch()
    command = 1
    pending = b""
    written = 0
    exiting = False
    os.set_blocking(1, False)
    with (root / "input").open("ab", buffering=0) as log:
        while True:
            path = root / f"command-{command}"
            if not pending and path.exists():
                payload = path.read_bytes()
                if payload == b"EXIT":
                    # Ordinary programs leave their terminal modes on exit.
                    payload = b"\x1b[?1002l\x1b[?1006l\x1b[?2004l\x1b[?1049l"
                    exiting = True
                pending = payload
                written = 0
            # A synchronous 4 MiB write would prevent this fixture from
            # reading input, measuring throughput instead of client fairness.
            readable, writable, _ = select.select(
                [] if (root / "pause-input").exists() else [0],
                [1] if pending else [], [], 0.01)
            if writable:
                try:
                    written += os.write(1, pending[written:written + 65536])
                except BlockingIOError:
                    pass
                if written == len(pending):
                    if exiting:
                        return 17
                    (root / "ack").write_text(str(command))
                    command += 1
                    pending = b""
            if readable:
                data = os.read(0, 65536)
                if not data:
                    return 0
                log.write(data)


class Session:
    def __init__(self, root, binary, initialized=True, baseline=None, disabled=False, borrow=False):
        self.root = root
        self.command = 0
        self.output = bytearray()
        self.status = None
        self.disabled = disabled
        self.baseline_replied = False
        self.baseline = baseline or {}
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
        env = dict(os.environ)
        env.pop("SSH_AUTH_SOCK", None)
        env.update(
            PATH=f"{bindir}:{env['PATH']}",
            XDG_CONFIG_HOME=str(root / "config"),
            XDG_RUNTIME_DIR=str(root / "runtime"),
            BEAMS_OVERLAY_FIXTURE=str(root),
            TERM="xterm-256color",
            BEAMS_OVERLAY_INITIALIZED="1" if initialized else "0",
            BEAMS_OVERLAY_BORROW="1" if borrow else "0",
        )
        # Every launch needs the workstation daemon. Run one per session so
        # it is isolated and reliably stopped.
        self.daemon = subprocess.Popen([binary, "daemon"], env=env,
                                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        socket = root / "runtime" / "beams-client" / "daemon.sock"
        deadline = time.monotonic() + 5
        while not socket.exists():
            if time.monotonic() >= deadline:
                self.daemon.kill()
                raise AssertionError("local daemon did not create its socket")
            time.sleep(0.02)
        self.pid, self.master = pty.fork()
        if self.pid == 0:
            try:
                (root / "tty").write_text(os.ttyname(0))
                fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
                (root / "before-termios").write_text(
                    json.dumps(termios.tcgetattr(0), default=list))
                command = [binary, "ssh", "--terminal-overlay",
                           "--purpose", "managed-session-test"]
                if disabled:
                    command.extend(["--escape-key", "none"])
                os.execve(binary, command, env)
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
        if (not self.disabled and not self.baseline_replied and b"\x1b[?4m\x1b[c" in self.output
                and (self.root / "ready").exists()):
            modes = [1, 5, 6, 7, 9, 25, 45, 47, 66, 69, 1000, 1001, 1002, 1003, 1004, 1005, 1006,
                     1015, 1016, 2004, 2026]
            reports = b""
            for mode in modes:
                enabled = self.baseline.get(mode, mode in (7, 25))
                value = 0 if enabled is None else 1 if enabled else 2
                reports += f"\x1b[?{mode};{value}$y".encode()
            reports += f"\x1b[4;{1 if self.baseline.get(4, False) else 2}$y".encode()
            reports += f"\x1b[20;{1 if self.baseline.get(20, False) else 2}$y".encode()
            reports += b"\x1bP1$r" + self.baseline.get("sgr", b"0") + b"m\x1b\\"
            reports += b"\x1bP1$r" + self.baseline.get("margins", b"1;24") + b"r\x1b\\"
            reports += f"\x1bP1$r{self.baseline.get('style', 0)} q\x1b\\".encode()
            reports += f"\x1b[?{self.baseline.get('kitty', 0)}u".encode()
            reports += f"\x1b[>4;{self.baseline.get('modifiers', 0)}m".encode()
            reports += b"\x1b[?62;22c"
            self.send(reports + self.baseline.get("input", b""))
            self.baseline_replied = True

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

    def takeover(self, text, label=b"Beams local menu", description="takeover menu"):
        """Ctrl-] where the overlay cannot restore the screen. The menu must
        still appear (taking the screen over), keys after the prefix belong
        to it, and closing pulses the remote size so it repaints."""
        baseline = self.remote_input()
        pulses = (self.root / "sizes").read_text().count("\n")
        offset = len(self.output)
        self.send(b"\x1d")
        self.wait(lambda: label in self.output[offset:], f"{description}: opens", timeout=2)
        if label == b"Beams local menu":
            # An approval key reaches the visible menu, never the remote.
            self.send(b"y")
        self.send(b"\x1d" + text)
        self.wait(lambda: self.remote_input() == baseline + b"\x1d" + text,
                  f"{description}: closes and only the literal prefix and later keys reach the remote")
        self.wait(lambda: (self.root / "sizes").read_text().count("\n") >= pulses + 2,
                  f"{description}: closing redraws the remote")

    def open(self, label=b"Beams local menu"):
        offset = len(self.output)
        self.send(b"\x1d")
        self.wait(lambda: label in self.output[offset:], "menu opening", timeout=2)

    def close(self):
        if getattr(self, "daemon", None) is not None:
            self.daemon.kill()
            self.daemon.wait()
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
        mode_baseline = session.remote_input()
        session.emit(b"\x1b[?1002$p\x1b[?1006$p\x1b[?47$p\x1b[?9999$p\x1b[4$p"
                     b"\x1b[?4m\x1b[?u\x1b[>0q")
        mode_replies = b"\x1b[?1002;1$y\x1b[?1006;1$y\x1b[?47;1$y\x1b[?9999;0$y\x1b[4;2$y"
        session.wait(lambda: session.remote_input() == mode_baseline + mode_replies,
                     "remote input-mode and unsupported mode reports")
        session.wait(lambda: b"\x1b[?4m" in session.output and b"\x1b[>0q" in session.output,
                     "physical keyboard and terminal version probes")
        keyboard_replies = b"\x1b[>4;2m\x1b[?1u\x1bP>|fixture qy\x1b\\"
        for byte in keyboard_replies:
            session.send(bytes([byte]))
        session.wait(lambda: session.remote_input() == mode_baseline + mode_replies + keyboard_replies,
                     "opaque split keyboard/version replies bypass menu actions")
        storm_baseline = session.remote_input()
        session.emit(b"\x1b[5n" * 512)
        session.wait(lambda: session.remote_input() == storm_baseline + b"\x1b[0n" * 512,
                     "query burst answered without queue loss")

        # Settings observe remote state at ST, before following state changes.
        # Aborting a DCS with ESC/CAN/SUB must never produce a reply.
        dcs_baseline = session.remote_input()
        session.emit(b"\x1b[31m\x1b[2;23r\x1bP$q")
        session.emit(b"m\x1b")
        session.emit(b"\\\x1b[32m\x1bP$qr\x1b\\\x1b[r"
                     b"\x1bP$q q\x1b\\\x1bP+q436f\x1b\\")
        dcs_replies = (b"\x1bP1$r0;31m\x1b\\\x1bP1$r2;23r\x1b\\"
                       b"\x1bP0$r\x1b\\\x1bP0+r\x1b\\")
        session.wait(lambda: session.remote_input() == dcs_baseline + dcs_replies,
                     "split DECRQSS remote-state and unsupported replies")
        session.emit(b"\x1bP$qm\x18\x1bP$qm\x1a\x1bP$qm\x1b[0m\x1b[5n")
        session.wait(lambda: session.remote_input() == dcs_baseline + dcs_replies + b"\x1b[0n",
                     "cancelled DECRQSS requests produce no replies")
        # Physical DCS replies are opaque even with menu keys in their body.
        physical_reply = b"\x1bP1$rqy\x1b\\"
        session.send(physical_reply[:6])
        session.send(physical_reply[6:])
        session.wait(lambda: session.remote_input().endswith(physical_reply),
                     "split physical DCS reply bypasses menu shortcuts")

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
        session.emit(b"\x1bc\x1b[?1049hREINITIALIZED AFTER RESIZE")
        session.wait(lambda: b"REINITIALIZED AFTER RESIZE" in session.output, "resize recovery")
        session.open()
        session.emit("\x1b[H界e\u0301 \x1b[7mdefault inverse\x1b[0m".encode())
        # A partial remote CSI must be replayed before passthrough resumes.
        session.emit(b"\x1b[38;2;")
        session.send(b"q")
        session.wait(lambda: session.output.endswith(b"\x1b[38;2;"), "unfinished CSI restored")
        session.emit(b"1;2;3mAFTER CLOSE\x1b[0m")
        session.wait(lambda: b"1;2;3mAFTER CLOSE" in session.output, "remote continuation")
        session.send(b"REMOTE INPUT")
        session.wait(lambda: session.remote_input().endswith(b"REMOTE INPUT"), "input after menu close")
        # Only the physical terminal answers in passthrough.
        passthrough_baseline = session.remote_input()
        passthrough_offset = len(session.output)
        session.emit(b"\x1bP$qm\x1b\\PASSTHROUGH QUERY")
        session.wait(lambda: b"\x1bP$qm\x1b\\PASSTHROUGH QUERY" in session.output[passthrough_offset:],
                     "passthrough DECRQSS reaches the physical terminal")
        session.send(b"\x1bP1$r0m\x1b\\")
        session.wait(lambda: session.remote_input() == passthrough_baseline + b"\x1bP1$r0m\x1b\\",
                     "exactly one physical DECRQSS reply in passthrough")
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
        ending = b"\x1b\\AFTER OVERSIZED"
        session.emit(ending)
        session.wait(lambda: oversized + ending in session.output[offset:],
                     "oversized OSC resumed byte-for-byte")
        assert session.output.count(oversized + ending) == 1, "oversized OSC duplicated"
        # Painting stays suppressed until an observed reset; the menu takes
        # the screen over instead.
        session.takeover(b"AFTER LIMIT", description="model lost to an oversized string")
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


def run_input_pressure(root, binary, outcome):
    session = Session(root, binary)
    try:
        session.open()
        paused = root / "pause-input"
        paused.touch()
        # More than the OS PTY can accept, but below the client's byte budget.
        queries = 8192
        session.emit(b"\x1b[5n" * queries + b"\x1b]2;PRESSURE DRAINED\x07")
        session.wait(lambda: b"PRESSURE DRAINED" in session.output,
                     "output and query dispatch while the child stops reading")
        if outcome == "resume":
            close_offset = len(session.output)
            session.send(b"qTYPED UNDER PRESSURE")
            session.wait(lambda: b"\x1b[?9l\x1b[?1000l" in session.output[close_offset:],
                         "menu closes while child input is blocked", timeout=2)
            fcntl.ioctl(session.master, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 90, 0, 0))
            session.wait(lambda: "30 90\n" in (root / "sizes").read_text(),
                         "menu input and resize while the child stops reading", timeout=2)
            session.emit(b"OUTPUT UNDER PRESSURE")
            session.wait(lambda: b"OUTPUT UNDER PRESSURE" in session.output,
                         "remote output stays responsive with blocked input", timeout=2)
            paused.unlink()
            expected = b"\x1b[0n" * queries + b"TYPED UNDER PRESSURE"
            session.wait(lambda: session.remote_input() == expected,
                         "queued query replies precede subsequent user input", timeout=10)
            session.command += 1
            (root / f"command-{session.command}").write_bytes(b"EXIT")
            session.wait(lambda: session.status is not None, "exit after input drain")
            assert session.status == 17
        elif outcome == "signal":
            os.kill(session.pid, signal.SIGTERM)
            session.wait(lambda: session.status is not None,
                         "SIGTERM remains responsive under input backpressure", timeout=2)
            assert session.status == 143
        elif outcome == "overflow":
            # Don't wait for fixture acknowledgement: the client terminates
            # the child before it can finish emitting an over-budget storm.
            session.command += 1
            (root / f"command-{session.command}").write_bytes(b"\x1b[5n" * (256 * 1024 // 4))
            session.wait(lambda: session.status is not None, "input budget exhaustion", timeout=10)
            assert session.status == 1
            assert b"child PTY input exceeded 256 KiB" in session.output
        session.assert_termios_restored()
    finally:
        session.close()

def run_unknown_and_recovery(root, binary):
    session = Session(root, binary, initialized=False)
    try:
        session.takeover(b"ORDINARY INPUT", description="unknown screen")
        session.emit(b"\x1bc\x1b[?1049hRECOVERED BASELINE")
        session.wait(lambda: b"RECOVERED BASELINE" in session.output, "observed initialization")
        session.open()
        session.send(b"f8080")
        session.wait(lambda: b"Forward a Beam port" in session.output, "form before recovery")
        offset = len(session.output)
        unsafe = b"\x1b_Ga=T,f=24;opaque qy\x1b\\AFTER UNSUPPORTED"
        session.emit(unsafe[:8])
        session.emit(unsafe[8:])
        session.wait(lambda: unsafe in session.output[offset:], "unsupported sequence replay after close")
        assert session.output[offset:].count(unsafe) == 1
        session.takeover(b"UNSEEN KEY", b"Forward a Beam port", "unsupported state")
        session.emit(b"\x1bc\x1b[?1049hRECOVERED AGAIN")
        session.wait(lambda: b"RECOVERED AGAIN" in session.output, "second baseline recovery")
        session.open(b"Forward a Beam port")
        session.wait(lambda: b"8080" in session.output[offset:], "form input preserved after recovery")
        os.kill(session.pid, signal.SIGTERM)
        session.wait(lambda: session.status is not None, "termination during recovered form")
        assert session.status == 143
        session.assert_termios_restored()
    finally:
        session.close()


def run_resize_reinitialization(root, binary):
    session = Session(root, binary)
    try:
        session.open()
        session.send(b"f8080")
        session.wait(lambda: b"Forward a Beam port" in session.output, "form before resize")
        session.send(b"\x1dBEFORE RESIZE INPUT")
        session.wait(lambda: session.remote_input().endswith(b"BEFORE RESIZE INPUT"),
                     "menu closed before resizing")
        fcntl.ioctl(session.master, termios.TIOCSWINSZ, struct.pack("HHHH", 12, 60, 0, 0))
        session.wait(lambda: "12 60\n" in (root / "sizes").read_text(), "uncovered PTY resize")

        # Margins, coordinates and a clear still lack known origin mode
        # after resize. (A saved slot may stay unknown; painting never uses it.)
        session.emit(b"\x1b[r\x1b[H\x1b[2JPARTIAL INITIALIZATION")
        session.wait(lambda: b"PARTIAL INITIALIZATION" in session.output, "partial initialization")
        session.takeover(b"DEFERRED AFTER RESIZE", b"Forward a Beam port",
                         "incomplete initialization")

        # No new RIS: the application explicitly initializes only this buffer.
        offset = len(session.output)
        before = b"\x1b[?6l\x1b[r\x1b[H\x1b7\x1b[2JREINITIALIZED WITHOUT RIS"
        session.emit(before)
        session.wait(lambda: b"REINITIALIZED WITHOUT RIS" in session.output[offset:],
                     "explicit buffer initialization")
        session.open(b"Forward a Beam port")
        session.wait(lambda: b"8080" in session.output[offset:], "form preserved across resize")
        hidden = b"\x1b[3;3H\x1b[31mRESIZED HIDDEN\x1b7\x1b[5;7H\x1b[6n"
        baseline = session.remote_input()
        session.emit(hidden)
        session.wait(lambda: session.remote_input() == baseline + b"\x1b[5;7R",
                     "remote cursor query after recovered menu")
        session.send(b"\x1dAFTER RESIZE INPUT")
        session.wait(lambda: session.remote_input().endswith(b"AFTER RESIZE INPUT"),
                     "close recovered menu")
        after = b"\x1b8!\x1b[0mAFTER RESIZED MENU"
        session.emit(after)
        session.wait(lambda: after in session.output[offset:], "saved state after recovered menu")
        if module := os.environ.get("BEAMS_TEST_XTERM_MODULE"):
            trace = dict(name="live explicit resize initialization", rows=12, cols=60,
                         initialRows=24, initialCols=100,
                         beforeResize=list(b"\x1bc\x1b[?1049h\x1b[?1002h\x1b[?1006h\x1b[?2004h"),
                         before=list(before), hidden=list(hidden), after=list(after),
                         physical=list(session.output[offset:]))
            subprocess.run(["node", str(Path(__file__).with_name("terminal-paint-reference.cjs")), module],
                           input=json.dumps([trace]), text=True, check=True)

        # A target buffer that has not been initialized must be selected raw,
        # after restoring the last safe buffer, even with a split mode frame.
        session.open(b"Forward a Beam port")
        offset = len(session.output)
        transition = b"\x1b[?00047lUNKNOWN PRIMARY"
        session.emit(transition[:7])
        session.emit(transition[7:])
        session.wait(lambda: transition in session.output[offset:], "unsafe buffer switch replay")
        assert session.output[offset:].count(transition) == 1
        session.takeover(b"UNKNOWN PRIMARY INPUT", b"Forward a Beam port", "uninitialized target")
        session.emit(before)
        session.wait(lambda: before in session.output[offset:], "initialize selected primary buffer")
        session.open(b"Forward a Beam port")
        os.kill(session.pid, signal.SIGTERM)
        session.wait(lambda: session.status is not None, "termination after primary initialization")
        assert session.status == 143
        session.assert_termios_restored()
    finally:
        session.close()


def inherited_initialization(alternate=True):
    # Deliberately explicit; an ordinary TUI clear does not prove these states.
    shared = (b"\x18\x1b\\\x1b<\x1b%G\x1b[?7h\x1b[?5;45;69l\x1b[20l\x1b[0\"q"
              b"\x1b(B\x1b)B\x0f\x1b[0m\x1b]8;;\x1b\\\x1b[4l"
              b"\x1b[?1l\x1b>\x1b[?25h\x1b[?2004l\x1b[?2026l"
              b"\x1b[?9;1000;1001;1002;1003l\x1b[?1005;1006;1015;1016l")
    select = b"\x1b[?47h" if alternate else b"\x1b[?47l"
    rows = b"".join(f"\x1b[{row};1H\x1b#5".encode() for row in range(1, 25))
    return shared + select + b"\x1b[3g\x1b[?6l\x1b[r" + rows + b"\x1b[H\x1b7\x1b[2J"


def run_inherited_initialization(root, binary):
    # The terminal does not recognize autowrap, so entry evidence cannot
    # supply it; the client never invents it. (Links, character sets and
    # the parser boundary are reset by the client at entry.)
    session = Session(root, binary, initialized=False, baseline={7: None})
    try:
        partial = inherited_initialization().replace(b"\x1b[?7h", b"")
        session.emit(partial + b"PARTIAL INHERITED INITIALIZATION")
        session.wait(lambda: b"PARTIAL INHERITED INITIALIZATION" in session.output,
                     "initialization with unknown autowrap")
        session.takeover(b"INHERITED INPUT", description="incomplete inherited initialization")

        offset = len(session.output)
        before = inherited_initialization() + b"KNOWN WITHOUT RIS"
        session.emit(before)
        session.wait(lambda: before in session.output[offset:], "complete inherited initialization")
        assert b"\x1bc" not in session.output, "fixture must not establish a RIS baseline"
        session.open()
        session.send(b"f8080")
        session.wait(lambda: b"Forward a Beam port" in session.output[offset:], "inherited menu form")
        hidden = (b"\x1b[3;3H\x1b[31m\x1b7"
                  b"\x1b]8;id=remote;https://example.test/remote\x1b\\LINK\x1b]8;;\x1b\\"
                  b"\x1b[4;1HA\tB\x1b[5;7H\x1b[6n\x1bP$qm\x1b\\")
        baseline = session.remote_input()
        session.emit(hidden)
        session.wait(lambda: session.remote_input() == baseline + b"\x1b[5;7R\x1bP1$r0;31m\x1b\\",
                     "known inherited cursor and attributes answer queries while covered")
        session.send(b"\x1dAFTER INHERITED INPUT")
        session.wait(lambda: session.remote_input().endswith(b"AFTER INHERITED INPUT"),
                     "inherited menu close")
        after = b"\x1b8!\x1b[0m\r\nAFTER INHERITED MENU"
        session.emit(after)
        session.wait(lambda: after in session.output[offset:], "saved state after inherited menu")
        if module := os.environ.get("BEAMS_TEST_XTERM_MODULE"):
            inherited = (b"UNSEEN WORKSTATION PRIMARY\x1b[1;31m\x1b7\x1b[?47h"
                         b"\x1b[?7l\x1b[?45h\x1b[4h\x1b[20h\x1b(0\x1b)0\x0e"
                         b"\x1b[?1003h\x1b[?1016h\x1b[?25l\x1b[?2004h"
                         b"\x1b]8;id=old;https://example.test/old\x1b\\"
                         b"\x1b]2;inherited unfinished string")
            trace = dict(name="live inherited initialization", rows=24, cols=100,
                         inherited=list(inherited), before=list(before), hidden=list(hidden),
                         after=list(after), physical=list(session.output[offset:]))
            subprocess.run(["node", str(Path(__file__).with_name("terminal-paint-reference.cjs")), module],
                           input=json.dumps([trace]), text=True, check=True)

        session.open(b"Forward a Beam port")
        offset = len(session.output)
        transition = b"\x1b[?00047lUNINITIALIZED PRIMARY"
        session.emit(transition[:7])
        session.emit(transition[7:])
        session.wait(lambda: transition in session.output[offset:], "inherited switch closes before replay")
        assert session.output[offset:].count(transition) == 1
        session.takeover(b"UNKNOWN BUFFER INPUT", b"Forward a Beam port", "unknown buffer")
        session.emit(inherited_initialization(False) + b"EXPLICIT PRIMARY")
        session.wait(lambda: b"EXPLICIT PRIMARY" in session.output[offset:], "primary explicitly initialized")
        session.open(b"Forward a Beam port")
        session.wait(lambda: b"8080" in session.output[offset:], "inherited form survives deferred switch")
        os.kill(session.pid, signal.SIGTERM)
        session.wait(lambda: session.status is not None, "inherited form termination")
        assert session.status == 143
        session.assert_termios_restored()
    finally:
        session.close()


def run_owned_borrowing(root, binary, outcome):
    session = Session(root, binary, initialized=False, borrow=True)
    try:
        before_resize = b"\x1bcPRIMARY SENTINEL\x1b[2;2H\x1b[31m\x1b7"
        session.emit(before_resize)
        session.wait(lambda: before_resize in session.output, "reset owns unused alternate")
        fcntl.ioctl(session.master, termios.TIOCSWINSZ, struct.pack("HHHH", 12, 80, 0, 0))
        session.wait(lambda: "12 80\n" in (root / "sizes").read_text(), "unknown primary after resize")
        partial = b"\x1b[?6l\x1b[r\x1b[2;3H"
        session.emit(partial)
        session.wait(lambda: partial in session.output, "partial primary geometry")
        session.takeover(b"UNKNOWN GEOMETRY INPUT", description="borrow without saved geometry")

        offset = len(session.output)
        before = partial + b"\x1b7\x1b[32mKNOWN CURSOR"
        session.emit(before)
        session.wait(lambda: before in session.output[offset:], "geometry without primary clear")
        session.open()
        assert b"\x1b[?47h" in session.output[offset:], "menu must select owned scratch"
        session.send(b"f8080")
        session.wait(lambda: b"Forward a Beam port" in session.output[offset:], "borrowed form")
        hidden = (b"\x1b[3;4H\x1b[35mRETAINED DRAWING\x1b7"
                  b"\x1b]8;id=remote;https://example.test/borrowed\x1b\\LINK\x1b]8;;\x1b\\")
        mark = len(session.output)
        session.emit(hidden)
        session.wait(lambda: b"\x1b[?2026l" in session.output[mark:], "borrowed output tick")
        assert hidden not in session.output[mark:], "drawing must remain off both physical buffers"
        baseline = session.remote_input()
        session.send(b"\x1dAFTER BORROWED INPUT")
        session.wait(lambda: session.remote_input() == baseline + b"\x1dAFTER BORROWED INPUT",
                     "manual borrowed close restores ordinary input")
        session.wait(lambda: hidden in session.output[mark:], "retained drawing replay")
        assert session.output[mark:].count(hidden) == 1
        after = b"\x1b8!\x1b[0mAFTER BORROWED MENU"
        session.emit(after)
        session.wait(lambda: after in session.output[offset:], "saved cursor after borrowed close")
        if module := os.environ.get("BEAMS_TEST_XTERM_MODULE"):
            trace = dict(name="live owned borrowed menu", rows=12, cols=80,
                         initialRows=24, initialCols=100, beforeResize=list(before_resize),
                         before=list(before), hidden=list(hidden), after=list(after),
                         physical=list(session.output[offset:]), checkInactive=True)
            subprocess.run(["node", str(Path(__file__).with_name("terminal-paint-reference.cjs")), module],
                           input=json.dumps([trace]), text=True, check=True)

        session.open(b"Forward a Beam port")
        session.wait(lambda: b"8080" in session.output[mark:], "borrowed form survives close")
        if outcome == "normal":
            session.command += 1
            (root / f"command-{session.command}").write_bytes(b"EXIT")
            session.wait(lambda: session.status is not None, "remote exit while borrowing")
            assert session.status == 17
            session.assert_termios_restored()
            return

        # A query closes before it is answered by the real primary terminal.
        mark = len(session.output)
        baseline = session.remote_input()
        drawing = b"\x1b[5;7HQUERY DRAWN"
        query = b"\x1b[6n"
        title = b"\x1b]2;BORROWED TITLE\x1b\\"
        clipboard = b"\x1b]52;c;QQ==\x1b\\"
        session.emit(drawing + query + title + clipboard)
        session.wait(lambda: drawing + query + title + clipboard in session.output[mark:],
                     "query closes and runs after retained drawing")
        assert session.remote_input() == baseline, "borrowed query must not also receive a model reply"
        assert session.output[mark:].count(query) == 1
        assert session.output[mark:].count(title) == 1
        assert session.output[mark:].count(clipboard) == 1
        session.send(b"\x1b[5;18R")
        session.wait(lambda: session.remote_input() == baseline + b"\x1b[5;18R",
                     "one physical query reply reaches the child")

        session.open(b"Forward a Beam port")
        mark = len(session.output)
        effect = b"\x1b]2;SPLIT BORROWED TITLE\x1b\\"
        session.emit(effect[:9])
        session.emit(effect[9:])
        session.wait(lambda: effect in session.output[mark:], "split effect closes before raw delivery")
        assert session.output[mark:].count(effect) == 1

        session.open(b"Forward a Beam port")
        mark = len(session.output)
        prefix = b"\x1b[38;2;"
        session.emit(prefix)
        session.wait(lambda: b"\x1b[?2026l" in session.output[mark:], "partial drawing retained")
        baseline = session.remote_input()
        session.send(b"\x1dAFTER PARTIAL INPUT")
        session.wait(lambda: session.remote_input() == baseline + b"\x1dAFTER PARTIAL INPUT",
                     "manual close during partial control")
        continuation = b"1;2;3mCOLOR AFTER BORROW\x1b[0m"
        session.emit(continuation)
        session.wait(lambda: prefix + continuation in session.output[mark:], "partial control resumes once")
        assert session.output[mark:].count(prefix + continuation) == 1

        session.open(b"Forward a Beam port")
        mark = len(session.output)
        burst = b"\x1b[H" + b"#" * (256 * 1024 + 8192) + b"BORROWED BUDGET COMPLETE"
        session.emit(burst)
        session.wait(lambda: burst in session.output[mark:], "borrowed retention budget closes without loss")
        assert session.output[mark:].count(b"#") == 256 * 1024 + 8192
        baseline = session.remote_input()
        session.send(b"AFTER BUDGET INPUT")
        session.wait(lambda: session.remote_input() == baseline + b"AFTER BUDGET INPUT",
                     "retention exhaustion returns to passthrough")

        session.emit(before)
        session.wait(lambda: before in session.output[mark:], "reset cursor after retained burst")
        session.open(b"Forward a Beam port")
        mark = len(session.output)
        transition = b"\x1b[?00047hREMOTE OWNS ALTERNATE"
        session.emit(transition[:8])
        session.emit(transition[8:])
        session.wait(lambda: transition in session.output[mark:], "borrow relinquished before remote selection")
        assert session.output[mark:].count(transition) == 1
        session.emit(b"\x1b[?47l" + before)
        session.wait(lambda: b"\x1b[?47l" + before in session.output[mark:], "remote returns to primary")
        session.takeover(b"NO SCRATCH INPUT", b"Forward a Beam port", "no owned scratch buffer")

        # A new RIS establishes a new scratch lifetime. Controlled covered
        # resize closes; this is not a workstation reflow guarantee.
        session.emit(before_resize)
        fcntl.ioctl(session.master, termios.TIOCSWINSZ, struct.pack("HHHH", 13, 80, 0, 0))
        session.wait(lambda: "13 80\n" in (root / "sizes").read_text(), "new scratch resize")
        session.emit(before)
        session.wait(lambda: before in session.output[mark:], "new scratch geometry")
        session.open(b"Forward a Beam port")
        mark = len(session.output)
        resize_drawing = b"\x1b[4;3HBEFORE BORROWED RESIZE"
        session.emit(resize_drawing)
        fcntl.ioctl(session.master, termios.TIOCSWINSZ, struct.pack("HHHH", 14, 80, 0, 0))
        session.wait(lambda: "14 80\n" in (root / "sizes").read_text(), "covered borrowed resize")
        session.wait(lambda: resize_drawing in session.output[mark:], "resize relinquishes and replays")
        assert session.output[mark:].count(resize_drawing) == 1
        session.emit(before)
        session.wait(lambda: before in session.output[mark:], "geometry after covered resize")
        session.open(b"Forward a Beam port")
        mark = len(session.output)
        signal_drawing = b"\x1b[6;2HBEFORE BORROWED SIGNAL"
        session.emit(signal_drawing)
        session.wait(lambda: b"\x1b[?2026l" in session.output[mark:], "drawing retained before signal")
        os.kill(session.pid, signal.SIGTERM)
        session.wait(lambda: session.status is not None, "SIGTERM while borrowing")
        assert signal_drawing in session.output[mark:]
        assert session.output[mark:].count(signal_drawing) == 1
        assert session.status == 143
        session.assert_termios_restored()
    finally:
        session.close()


def run_lifecycle(root, binary, outcome):
    disabled = outcome == "disabled"
    settings = {25: False, 47: True, 66: True, 1: True, 4: True, 6: True,
                1002: True, 1006: True, 2004: True, "sgr": b"0;1;31",
                "kitty": 9, "modifiers": 2, "margins": b"2;20",
                "input": b"EARLY USER INPUT", "style": 5}
    session = Session(root, binary, baseline=settings, disabled=disabled)
    try:
        if disabled:
            assert b"$p" not in session.output and b"\x1bP$q" not in session.output
            before = len(session.output)
            user = b"literal\x1d\x1b[?9u\x1b[27;5;93~\x1b]reply qy\x1b\\"
            session.send(user)
            session.wait(lambda: session.remote_input() == user, "strict disabled input passthrough")
            raw = b"\x1b]2;DISABLED TITLE\x1b\\\x1b[6n\x1bP$qm\x1b\\DISABLED OUTPUT"
            session.emit(raw)
            session.wait(lambda: raw in session.output[before:], "strict disabled output passthrough")
            session.pump()
            assert bytes(session.output[before:]) == raw, "disabled session injected local output"
            # Exit while a physical OSC remains unfinished: strict passthrough
            # must not add CAN/ST or mode resets on termination.
            partial = b"\x1b]0;STRICT PARTIAL"
            session.emit(partial)
            session.wait(lambda: partial in session.output[before:], "strict partial string")
            os.kill(session.pid, signal.SIGTERM)
            session.wait(lambda: session.status is not None, "strict SIGTERM")
            assert bytes(session.output[before:]) == raw + partial
        else:
            session.wait(lambda: session.remote_input() == b"EARLY USER INPUT", "input retained during probes")
            if outcome == "signal":
                session.open()
                session.emit(b"\x1b]0;UNFINISHED EXIT STRING")
                os.kill(session.pid, signal.SIGTERM)
            else:
                session.command += 1
                (root / f"command-{session.command}").write_bytes(b"EXIT")
            session.wait(lambda: session.status is not None, "baseline lifecycle exit")
            tail = session.output[-1024:]
            for mode in [b"\x1b[?25l", b"\x1b[?47h", b"\x1b=", b"\x1b[?1h",
                         b"\x1b[4h", b"\x1b[?6h", b"\x1b[2;20r", b"\x1b[?1002h",
                         b"\x1b[?1006h", b"\x1b[?2004h", b"\x1b[0;1;31m",
                         b"\x1b[=9u", b"\x1b[>4;2m", b"\x1b[5 q"]:
                assert mode in tail, f"nondefault baseline not restored: {mode!r}"
            assert b"\x1b[<" not in tail, "cleanup popped an unowned Kitty stack"
            if module := os.environ.get("BEAMS_TEST_XTERM_MODULE"):
                trace = dict(name=f"live {outcome} baseline cleanup", rows=24, cols=100,
                             physical=list(session.output), expectedModes={
                                 "mouseTrackingMode": "drag", "applicationCursorKeysMode": True,
                                 "applicationKeypadMode": True, "bracketedPasteMode": True,
                                 "insertMode": True, "originMode": True},
                             expectedPen={"bold": True, "fg": 1}, expectedBuffer="alternate",
                             expectedHidden=True)
                subprocess.run(["node", str(Path(__file__).with_name("terminal-paint-reference.cjs")), module],
                               input=json.dumps([trace]), text=True, check=True)
        assert session.status == (143 if outcome != "normal" else 17)
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
        recovery = root / "recovery"
        recovery.mkdir()
        run_unknown_and_recovery(recovery, binary)
        resized = root / "resized"
        resized.mkdir()
        run_resize_reinitialization(resized, binary)
        inherited = root / "inherited"
        inherited.mkdir()
        run_inherited_initialization(inherited, binary)
        for outcome in ["signal", "normal"]:
            borrowed = root / f"borrowed-{outcome}"
            borrowed.mkdir()
            run_owned_borrowing(borrowed, binary, outcome)
        for outcome in ["normal", "signal", "disabled"]:
            lifecycle = root / f"lifecycle-{outcome}"
            lifecycle.mkdir()
            run_lifecycle(lifecycle, binary, outcome)
        for outcome in ["resume", "signal", "overflow"]:
            pressure = root / f"pressure-{outcome}"
            pressure.mkdir()
            run_input_pressure(pressure, binary, outcome)
        interrupted = root / "interrupted"
        interrupted.mkdir()
        session = Session(interrupted, binary)
        try:
            session.open()
            os.kill(session.pid, signal.SIGTERM)
            session.wait(lambda: session.status is not None, "handled SIGTERM")
            assert session.status == 143, f"wrong SIGTERM status: {session.status}"
            session.assert_termios_restored()
            for mode in [b"\x1b[?1002l", b"\x1b[?1006l", b"\x1b[?2004l", b"\x1b[?47l"]:
                assert mode in session.output[-512:], f"mode cleanup missing on SIGTERM: {mode!r}"
        finally:
            session.close()
    print("terminal-overlay protocol/PTY smoke test passed")


if __name__ == "__main__":
    if sys.argv[1:2] == ["--fake-tsh"]:
        sys.exit(fake_tsh(sys.argv[2:]))
    main()
