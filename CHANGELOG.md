# Changelog

## Unreleased

- Include operator hook messages stored in per-user journals in event history,
  follow, and follow cursors. Keep the system-journal access check so a partial
  unprivileged view is still reported as needing root.

- Fix adoption retirement to validate the legacy hook against the rendered
  replacement, including interrupted retirement and a no-op second init.
- Supply trusted system executable directories when a privileged runner omits
  PATH; preserve explicitly configured paths. Keep non-root status, BIOS and
  proof views usable with private state, reporting evidence as needing root.
  Status JSON carries reminders inside its document. Strict action readers and
  private directory permissions are unchanged.

- Resume interrupted init group creation before observing dependent, still-planned
  memberships. Started and completed membership checks still reject a missing
  group; each planned operation is checked when it executes in journal order.

- Finish the docs front door: a committed `just demo` screenshot, quickstart,
  the six safety rules, the command table, and install-on-a-new-machine
  guidance. Add getting-started, design, architecture (Figure 1), and outage
  timeline pages, and cross-link the rest of the docs.
- Generate `docs/reference/cli/*.md` and `man/*.1` through `just docs-gen`
  (`NUT_UPDATE_GOLDENS=1`) with drift tests. Extend `just install-smoke` to a
  read-only invocation of every command group plus man-page presence, and add
  `just install` for the root-owned binary, man pages, and user completion.
- Add `release.yml` on `v*` tags: a static `x86_64-unknown-linux-musl` binary
  with a SHA-256 checksum attached to the GitHub Release. Cut `v0.1.0` only
  after athena adoption.

- Authenticate UPS actions on the least-privilege operator credential:
  `nut beeper [mute|enable|disable]` shows or changes the beeper and confirms
  the new `ups.beeper.status`, `nut battery test [--stop]` runs or aborts a
  guarded quick self-test behind an interactive terminal and a `[y/N]`
  confirmation with Ctrl-C sending `test.battery.stop`, and `nut notify [test]`
  shows recipients and recent hook events or runs the hook's `test-notify`
  event. The client speaks `USERNAME`/`PASSWORD` with `SET TRACKING ON` and
  polls `GET TRACKING`, and a submission timeout stays "outcome unknown"
  without retrying. Secrets never reach argv, logs, or JSON. The e2e job
  proves the stanza allowlist, the live `load.off`/`shutdown.return`
  refusals, the driver-boundary beeper admission, the terminal interlock, and
  the notify test mail.
- Run the dummy-ups e2e job for real: build the locked release binary as the
  unprivileged runner and pass it to the root harness through `NUT_E2E_BIN`,
  calling `just` by absolute path because sudo's `secure_path` hid it.
  Install the runner shutdown/sendmail stubs by renaming the originals aside
  so the usrmerge `/sbin/shutdown -> systemctl` symlink no longer gets
  `systemctl` overwritten by the stub, make the disposable runner's fstab
  pass `findmnt --verify` before init verification, keep `findmnt` output in
  failure diagnostics, and stop the `MONITOR` redaction from mangling doctor
  prose.
- Start dummy-ups for real: its rendered `ups.conf` stanza no longer carries
  `offdelay`/`ondelay`, which NUT 2.8.1 dummy-ups rejects as invalid driver
  variables. Give init a bounded settle window (about 15 s) for Ubuntu's
  `Type=simple` upsd and upsmon units: the monitor's fresh safe-online gate
  re-observes unavailable or stale readings, and post-apply verification
  repeats while its only failures are readiness gaps. Explicit OB, LB, or FSD
  readings and every other failure still refuse immediately.
- Treat upsd's connect-time `ups.status: WAIT` placeholder, which NUT 2.8.1
  publishes until the driver answers its first dump, as no UPS data: doctor
  `hw.data` fails with a "waiting for the driver" reason, and init's
  safe-online gate and post-apply verification settle through it instead of
  refusing a just-restarted stack as not on line power. Rollback no longer
  needs a fresh upsd reading to stop the server and driver once the monitor
  is stopped, so restoring an all-inactive pre-init stack no longer refuses
  after it stops upsd; starts and every stop under a running monitor keep the
  safe-online gate.
- Stop the units init started before rollback restores configuration files,
  and start pre-init-active units only after the restore. Restoring the
  package `ups.conf` first left the driver's `ExecStop` unable to find its
  device, so the driver stopped as `failed` and rollback refused. A unit that
  stops as `failed` or had already failed is cleared with
  `systemctl reset-failed` instead of blocking rollback.
- Keep drill records inside the protected init state tree: `nut drill` now
  publishes records atomically as `root:root` 0600 files under the 0700
  `/var/lib/nut-cli/drills`, and non-root `nut doctor` skips the root-only
  proof checks with a `sudo nut doctor` hint instead of reporting them as
  unknown once `nut init` has secured `/var/lib/nut-cli`.
- Prove init, dummy-ups events, and rollback on real NUT 2.8.1: isolated
  Ubuntu 24.04 `e2e.yml` installs the root-owned `/usr/local/bin/nut`, runs
  hash-bound `init --yes --expect-plan` plus the documented root `init --yes`
  path, asserts dummy USB and `shutdown.return` skips, a no-op second init,
  dummy OB/OL status and events, and rollback that restores files while
  retaining packages. `just check` stays offline via `just e2e-lint` and
  fixture tests. Skip `hw.shutdown-return` for dummy-ups, treat
  `static`/`indirect` systemd enablement as enabled, and document privileged
  previews, secret rotation, recovery, and the read-only athena dry-run
  preview. See `docs/init.md` and `docs/development.md`.
- Bind reviewed plans to privileged CLI execution: `nut init` with
  `-n/--dry-run`, `--rotate-secrets`, `--expect-plan HASH`, and `-y/--yes` plus
  `nut rollback [STAMP]` with `-l/--list`, `-n/--dry-run`, and `-y/--yes`.
  Interactive runs preview before applying, non-root elevation requires a
  terminal and the root-owned `/usr/local/bin/nut` with safe ancestors through
  the exact `sudo --` argv, and list/dry-run never mutate or elevate. JSON is
  schema-versioned and redacted, help and completion cover the new flags, and
  width 60/80/120 Unicode/ASCII screens are snapshot-tested. Conflicting flags
  use exit 2; operational refusals use exit 1.
- Add validated init rollback planning and restoration with the shared writer
  lock, resumable private progress records, byte/metadata restoration, absent
  target removal, exact membership reversal, and safe cleanup of an unused
  transaction-created operator group. Fresh OL/no-OB/LB/FSD evidence gates
  service activity; packages and package-owned users remain installed. Add
  doctor `state.drift` and `state.backup` checks for applied outputs and backup
  availability, and enforce the exact operator command allowlist and CLI file
  ownership/modes. See `docs/init-rollback.md`.
- Add safe init convergence orchestration: root/applicability/reviewed-hash
  validation under the shared writer lock, coordinated transaction and
  resource operations, service dependency effects in driver/server/monitor
  order with fresh safe-online gates, a fast structured doctor pass that never
  parses terminal output, failed-verification compensation from recorded
  preimages with the exact `nut rollback <stamp>` hint, and idempotent state
  finalization. A second identical init reports zero changes; restarts always
  execute so units pick up new configuration, and already-satisfied enables
  stay no-ops. Adopted credentials are retained byte-for-byte while legacy
  operator grants are repaired; rotation regenerates the pair once per
  transaction and retries reuse the same secrets. Correct multi-directive
  operator grants no longer force a perpetual upsd.users rewrite.

- Add typed, journal-bound init resource application: simulated and version-pinned
  missing-only package installs with NUT startup suppression, additive operator
  groups, selected USB-node repair, atomic managed files, validated nofail edits,
  and proven legacy-hook retirement. Interrupted operations reconcile their
  actual state before retrying; package policies and file preimages survive
  failure. Config aliases and formatting do not force repeated config copies;
  ownership drift, operator credentials and group creation have explicit plans.

- Add a durable init transaction store with private backup manifests and blobs,
  absent-target markers, shared writer locking, checkpoint reconciliation,
  one-time private secret materialization, and idempotent state finalization.
  Interrupted operations are reconciled against their before/after fingerprints;
  corrupt or out-of-scope backup data fails closed, and public backup summaries
  contain no file contents or password digests.
- Add `nut init` planning with typed observations and a pure deterministic
  planner: explicit package, device, file, membership, fstab, retirement,
  service, and state steps, secret adoption and rotation intents, stable
  SHA-256 plan hashes, partial previews, and least-privilege rendering. Dummy
  profiles resolve without fictional USB IDs; the operator account grants only
  the five approved `instcmds` directives with no `actions` grant.
- Extend `Host` with protected no-follow atomic replacement and removal, secure
  directories, exclusive apply locking, deterministic entropy, bounded argv
  execution with secret-safe errors and inherited-TTY spawning, and injectable
  UPS observations reused by doctor. The test fake records effects, queues
  command and UPS results, and injects operation failures without touching the
  OS or port 3493.
- Ship `nut doctor` with a 45-check registry, JSON, `--strict`/`--observe`/`--list`,
  positional ID and section filters, generated `docs/doctor-checks.md`, and a
  `POWEROFF_WAIT` warning against full-charge runtime plus 10 minutes.
- Extend `Host` with metadata, current executable, and recorded sleep; extend the
  upsd client with greeting/version, `LIST CMD`, and `LIST CLIENT`.

- Ship `nut events` with bounded journal JSON reads, outage/blip/shutdown and
  communication stories, legacy hook continuity, raw/filter/follow modes, and
  explicit journal-access guidance.
- Add optional last-outage and current on-battery evidence to local status.
- Preserve unknown historical charge/mail delivery and incomplete boot/outage
  sequences; add property tests, redacted fixtures, and width/ASCII snapshots.

- Add DMI-matched board profiles (`asus-prime-trx40-pro` and `generic`), efivar
  fingerprints, a `BiosBackend` trait with manual and firmware-attributes backends,
  and the OS boot-readiness audit.
- Ship `nut bios` with `guide`, `setup`, `attest`, and `profile list|show|new`.
- Remind on bare `nut` while a BIOS attestation is pending.
- Add `just remote` for read-only commands on a named host.
- Finish the `schema = 1` host config (`[ups.<name>]`, shutdown, notify, boot, operators)
  and embed CyberPower, usbhid-generic, and dummy UPS profiles.
- Add typed renderers for `nut.conf`, `ups.conf`, `upsd.users`, `upsmon.conf`,
  `upssched.conf`, and the POSIX `nut-upssched-cmd` hook.
- Ship `nut config new|render|diff` with secret masking and a redacted athena golden.
- Bootstrap the `nut-cli` Rust package with the `nut` executable and `nut_cli` library.
- Add help/version output, offline CLI tests, and the documented safety contract.
- Pin Rust, commit the dependency lockfile, and add the Justfile and SHA-pinned CI gates.
- Register the SASE `check` tool with gate inputs and toolchain fingerprints.

## Completion

- Add live protocol-1 bash/zsh completion, grouped command descriptions, bounded UPS-name lookup, board profiles and backup stamps, and atomic adapter install/status/uninstall with real-shell CI tests.
