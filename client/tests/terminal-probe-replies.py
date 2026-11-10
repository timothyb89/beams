#!/usr/bin/env python3
"""Record a terminal's replies to the overlay's entry probes.

Run this directly in the terminal under test, outside beams, ssh, tmux and
zellij (each of those answers some probes itself):

    python3 client/tests/terminal-probe-replies.py replies.bin

It sends exactly the queries `beams ssh --terminal-overlay` sends at session
entry (`Baseline::probes` in client/src/session/baseline.rs; a unit test keeps
the two lists identical), waits for the answers, restores the terminal and
writes the raw reply bytes to the given file. Do not type while it runs: input
arriving in that window would be recorded as well.

See "Adding terminal recordings" in docs/development/terminal-overlay.md.
"""

import os
import re
import select
import sys
import termios
import time
import tty

MODES = [1, 5, 6, 7, 9, 25, 45, 47, 66, 69, 1000, 1001, 1002, 1003, 1004, 1005, 1006, 1015, 1016, 2004, 2026]
# Sent first so the client can tell, before forwarding anything, whether the
# terminal answers positioning queries at all (the entry scrollback push).
CPR = b"\x1b[6n"
EXTRA = b"\x1b[4$p\x1b[20$p\x1bP$qm\x1b\\\x1bP$qr\x1b\\\x1bP$q q\x1b\\\x1b[?u\x1b[?4m\x1b[c"

# The client waits for the DA1 sentinel (the last probe) this long before
# forwarding remote output; later replies are still claimed for 5 s.
CLIENT_WINDOW = 0.5


def probes():
    return CPR + b"".join(f"\x1b[?{mode}$p".encode() for mode in MODES) + EXTRA


def main():
    if len(sys.argv) != 2:
        sys.exit(f"usage: {sys.argv[0]} REPLIES.bin")
    fd = os.open("/dev/tty", os.O_RDWR | os.O_NOCTTY)
    saved = termios.tcgetattr(fd)
    data, last = b"", None
    try:
        tty.setraw(fd)
        start = time.monotonic()
        os.write(fd, probes())
        # Stop 0.5 s after the latest byte, or after 3 s regardless.
        while True:
            now = time.monotonic()
            if now - start > 3 or (last is not None and now - last > 0.5):
                break
            if select.select([fd], [], [], 0.05)[0]:
                chunk = os.read(fd, 4096)
                if not chunk:
                    break
                data += chunk
                last = time.monotonic()
    finally:
        termios.tcsetattr(fd, termios.TCSAFLUSH, saved)
        os.close(fd)
    with open(sys.argv[1], "wb") as out:
        out.write(data)
    print(f"wrote {len(data)} reply bytes to {sys.argv[1]}")
    for key in ["TERM_PROGRAM", "TERM_PROGRAM_VERSION", "TERM"]:
        print(f"{key}={os.environ.get(key, '')}")
    if last is None:
        print("no replies: this terminal answers none of the probes")
    else:
        elapsed = last - start
        note = "" if elapsed <= CLIENT_WINDOW else f" (later than the client's {CLIENT_WINDOW * 1000:.0f} ms window)"
        print(f"last reply after {elapsed * 1000:.0f} ms{note}")
    # One line per reply: split before each ESC that is not a terminator.
    for reply in re.split(rb"(?=\x1b(?!\\))", data):
        if reply:
            print("  " + repr(reply)[2:-1])


if __name__ == "__main__":
    main()
