# Authenticated UPS actions

Reads stay anonymous. Anything that changes UPS state runs as the
least-privilege `nut-operator` account that `nut init` converges: the only
grants in `upsd.users` are `beeper.disable`, `beeper.enable`, `beeper.mute`,
`test.battery.start.quick`, and `test.battery.stop` — never `ALL`, never
`load.*`, never `shutdown.*`. The server enforces the allowlist; this client
only ever sends those five commands.

```sh
nut beeper                 # show the beeper state (read-only)
nut beeper mute            # mute until the next UPS event
nut beeper enable          # sound the beeper on alarms
nut beeper disable         # silence the beeper entirely
nut battery test           # run a guarded quick self-test (interactive)
nut battery test --stop    # abort a running test (interactive)
nut notify                 # show recipients and recent hook notifications
nut notify test            # send a test notification through the hook
```

## Operator credential

The credential lives in `/etc/nut-cli/operator.secret`, installed
`root:nut-operators 0640` by `nut init`. It never enters argv, logs, diffs,
`--verbose`, or JSON: the CLI reads the protected file and speaks it only
inside the `PASSWORD` protocol line. When the file is unreadable, the error
names the group membership that unlocks it — join `nut-operators` and log back
in, or run with `sudo` — and never the secret. When the file is missing, the
remedy is `sudo nut init`.

## Protocol

Every verb logs in with `USERNAME`/`PASSWORD`, enables `SET TRACKING ON`, and
sends `INSTCMD` on that session, then polls `GET TRACKING` until the driver
reports `SUCCESS`, exactly like `upscmd -w`. A submission timeout means
"outcome unknown": the command may still run, so it is never retried
automatically — check `nut battery --all` and decide. Logout is best-effort;
closing the connection already discards the server-side session.
Lost or malformed acknowledgements and lost tracking replies have the same
unknown outcome; an explicit server refusal remains a failure.

## Beeper

Bare `nut beeper` reads `ups.beeper.status` without authenticating and reports
`Not reported` when the driver does not publish it. Verbs authenticate, send
the matching command, and re-read `ups.beeper.status` to confirm the new
state; a mismatch is an operational failure, not a silent success.

## Battery test

`nut battery test` needs an interactive terminal on stdin and stdout, so
agents, pipes, and cron cannot start or stop one. Starting also needs a
`[y/N]` confirmation and explains that the UPS briefly runs on battery. After
the driver accepts the command, the CLI polls `ups.test.result` with a live
spinner until the value leaves its in-progress state. Ctrl-C sends
`test.battery.stop` before giving up; `--stop` aborts a running test without
asking. A result that never settles, or a driver that never reports one, is
an operational failure with the command that shows the live state.
An unchanged pre-test result cannot prove the new test finished: the CLI
requires a changed result or an observed in-progress transition, even if
the new test ends with the same result as the previous one.

## Notify

Bare `nut notify` shows the `notify.from`/`notify.to` recipients from the host
config plus the five most recent `nut-upssched` hook entries, newest first.
Without journal access it still shows the recipients, prints the same
`sudo nut notify` guidance as `nut events`, and exits 1 with a JSON
`access: denied` document under `--json`. `nut notify test` runs the
installed hook's `test-notify` event — no privilege, no terminal, agents may
run it — and says to confirm delivery in the inbox, because submission is not
delivery. A missing hook points at `sudo nut init`.

## Evidence

Offline, the scripted fake models the operator account, the allowlist, and
tracking, including a fake-only stuck-`PENDING` mode that exercises the
submission-timeout path and scripted `ups.test.result` progressions. On real
NUT 2.8.1 with `dummy-ups` (which implements no instant commands), the
disposable e2e job proves three things: the rendered stanza carries exactly
the five allowed commands, the live operator credential is refused `load.off`
and `shutdown.return`, and a beeper probe receives the unsupported-command
response. The probe reads the password from its file and verifies
`driver.name = dummy-ups` before sending any instant command. Because
dummy-ups may reject unsupported commands before checking authorization,
the exact rendered stanza provides the ACL proof in that case. It also proves
`nut beeper` degrades gracefully, `nut battery test` refuses to run without a
terminal, and `nut notify test` delivers mail through the stub.
