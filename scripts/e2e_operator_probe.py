"""Probe operator ACLs only on a disposable dummy-ups server.

Read the password directly from its file: NUT 2.8.1's upscmd -p would expose
it in process arguments. Never include authentication replies in diagnostics.
"""

import socket
import sys
from pathlib import Path


def probe(command, secret_path=Path("/etc/nut-cli/operator.secret"), port=3493):
    if command not in ("beeper.mute", "load.off", "shutdown.return"):
        raise ValueError("unsupported e2e probe")
    secret = secret_path.read_text().rstrip("\n\r")
    if not secret or len(secret) > 512 or any(ord(c) < 32 or ord(c) == 127 for c in secret):
        raise ValueError("invalid operator credential")
    quoted = '"' + secret.replace("\\", "\\\\").replace('"', '\\"') + '"'
    with socket.create_connection(("127.0.0.1", port), timeout=2) as connection:
        with connection.makefile("rb") as response:
            def request(line):
                connection.sendall((line + "\n").encode())
                reply = response.readline(4097)
                if reply.startswith(b"Network UPS Tools upsd "):
                    reply = response.readline(4097)
                if len(reply) > 4096 or not reply.endswith(b"\n"):
                    raise ValueError("invalid upsd response")
                return reply.decode().strip()

            # A second guard beyond the harness's isolated-runner checks.
            if request("GET VAR dummy driver.name") != 'VAR dummy driver.name "dummy-ups"':
                raise ValueError("refusing operator probes on a non-dummy driver")
            for line in ("USERNAME nut-operator", "PASSWORD " + quoted):
                if request(line) != "OK":
                    raise ValueError("operator authentication failed")
            if request("SET TRACKING ON") != "OK" or request("GET TRACKING") != "ON":
                raise ValueError("operator tracking was not enabled")
            reply = request("INSTCMD dummy " + command)
            # Dummy Mode implements no commands. A known refusal plus the
            # exact stanza assertion is the proof; never echo arbitrary replies.
            if reply not in ("ERR ACCESS-DENIED", "ERR CMD-NOT-SUPPORTED"):
                raise ValueError("operator probe did not return an expected refusal")
            return reply.lower()


if __name__ == "__main__":
    try:
        if len(sys.argv) != 2:
            raise ValueError("expected one command")
        print(probe(sys.argv[1]))
    except (OSError, ValueError, UnicodeError):
        sys.exit("operator probe failed; inspect the disposable NUT service")
