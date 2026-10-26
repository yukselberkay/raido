#!/usr/bin/env python3
"""Exercise the compiled application in a real PTY, using only Python stdlib."""
import fcntl
import os
from pathlib import Path
import pty
import select
import signal
import struct
import sys
import tempfile
import termios
import time


def main():
    binary = str(Path(sys.argv[1] if len(sys.argv) > 1 else "target/release/raido").resolve())
    with tempfile.TemporaryDirectory(prefix="raido-smoke-") as directory:
        root = Path(directory)
        source = root / "main.rs"
        source.write_text("fn main() {}\n", encoding="utf-8")
        pid, fd = pty.fork()
        if pid == 0:
            os.chdir(root)
            os.environ.update(TERM="xterm-256color", SHELL="/bin/sh", XDG_CONFIG_HOME=str(root / "config"))
            # Run standalone even when started from inside a Raido terminal pane.
            os.environ.pop("RAIDO_SOCKET", None)
            os.execv(binary, [binary, str(source)])
        output = bytearray()
        exited = False

        def drain(seconds=0.3):
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                ready, _, _ = select.select([fd], [], [], min(0.04, max(0, deadline-time.monotonic())))
                if ready:
                    try:
                        data = os.read(fd, 65536)
                    except OSError:
                        return
                    if not data:
                        return
                    output.extend(data)

        def send(value):
            os.write(fd, value)
            drain()

        def prefix(value):
            send(b"\x02")
            send(value)

        try:
            fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 150, 0, 0))
            os.kill(pid, signal.SIGWINCH)
            drain(0.8)
            assert b"RAIDO" in output, "Application did not render"
            send(b"i")
            send(b"\x1b[200~// smoke \xce\xbb\n\x1b[201~")
            send(b"\x1b")
            send(b"\x13")
            assert source.read_text() == "// smoke λ\nfn main() {}\n", "Edit/save failed"
            send(b":2\r")
            send(b"/main\r")
            prefix(b"%")
            prefix(b"c")
            send(b"printf 'RAIDO_PTY_OK\\n'\r")
            drain(0.5)
            assert b"RAIDO_PTY_OK" in output, "Nested terminal did not render"
            # `raido FILE` in Raido's own terminal opens FILE here; no second Raido.
            send(f"'{binary}' fromshell.txt\r".encode())
            drain(1.0)
            assert "fromshell.txt ─".encode() in output, "raido FILE nested instead of opening a pane"
            send(b"ihello from the shell")
            send(b"\x1b")
            send(b"\x13")
            assert (root / "fromshell.txt").read_text() == "hello from the shell", "Opened file was not editable"
            prefix(b"a")
            assert b"YOUR TOOLS" in output, "AI launcher did not render"
            fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 28, 100, 0, 0))
            os.kill(pid, signal.SIGWINCH)
            drain()
            prefix(b":")
            send(b"qa\r")
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                waited, status = os.waitpid(pid, os.WNOHANG)
                if waited:
                    exited = True
                    assert os.waitstatus_to_exitcode(status) == 0, "App exited with an error"
                    break
                drain(0.05)
            assert exited, "App failed to exit"
            assert b"\x1b[?1049l" in output, "Alternate screen was not restored"
            assert not (termios.tcgetattr(fd)[3] & termios.ICANON) == 0, "Raw mode was not restored"
            print("PASS: real PTY startup, Unicode paste, save, search, jump, split, nested shell, raido FILE from the shell, AI launcher, resize, clean exit and terminal restoration")
        finally:
            if not exited:
                try:
                    os.kill(pid, signal.SIGTERM)
                    os.waitpid(pid, 0)
                except ProcessLookupError:
                    pass
            os.close(fd)


if __name__ == "__main__":
    main()
