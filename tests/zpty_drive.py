#!/usr/bin/env python3
"""Drive real zsh Tab completion via pty with deadline loops (no fixed sleeps for reads)."""
import os
import pty
import select
import sys
import time

def read_until(fd, marker: bytes, deadline: float) -> bytes:
    out = b""
    while time.time() < deadline:
        r, _, _ = select.select([fd], [], [], 0.2)
        if r:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                break
            if not chunk:
                break
            out += chunk
            if marker in out:
                break
        # No fixed sleep: select already waits up to 0.2s.
    return out

def read_all(fd, deadline: float) -> bytes:
    out = b""
    while time.time() < deadline:
        r, _, _ = select.select([fd], [], [], 0.2)
        if not r:
            # Quiet for 0.2s; keep waiting until the deadline.
            continue
        try:
            chunk = os.read(fd, 65536)
        except OSError:
            break
        if not chunk:
            break
        out += chunk
    return out

def main() -> int:
    home = sys.argv[1]
    bindir = sys.argv[2]
    adapter = sys.argv[3]
    keys = sys.argv[4].encode().decode("unicode_escape").encode()
    columns = sys.argv[5]
    extra = sys.argv[6] if len(sys.argv) > 6 else ""
    pid, fd = pty.fork()
    if pid == 0:
        os.environ["HOME"] = home
        os.environ["TERM"] = "xterm-256color"
        os.environ["PATH"] = f"{bindir}:/usr/bin:/bin"
        os.environ["COLUMNS"] = columns
        os.execvp("zsh", ["zsh", "-f", "-i"])
        return 1
    deadline = time.time() + 10
    # Wait for the first prompt.
    read_until(fd, b"%", deadline)
    setup = (
        f"stty cols {columns} rows 50\n"
        "PS1='> '\n"
        f"fpath=({adapter} $fpath)\n"
        "autoload -Uz compinit\n"
        "compinit -D -u\n"
        "zmodload zsh/complist\n"
    )
    if extra:
        setup += extra + "\n"
    setup += "echo MARKER_READY\n"
    os.write(fd, setup.encode())
    out = read_until(fd, b"MARKER_READY", time.time() + 10)
    if b"MARKER_READY" not in out:
        print(out.decode(errors="replace")[-2000:])
        return 2
    # Drain any pending listing state, then send the keys.
    os.write(fd, keys)
    out = read_all(fd, time.time() + 10)
    sys.stdout.buffer.write(out)
    return 0

if __name__ == "__main__":
    raise SystemExit(main())
