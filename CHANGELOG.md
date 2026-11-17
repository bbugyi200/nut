# Changelog

## Unreleased

- Extend `Host` with protected no-follow atomic replacement and removal, secure
  directories, exclusive apply locking, deterministic entropy, bounded argv
  execution with secret-safe errors and inherited-TTY spawning, and injectable
  UPS observations reused by doctor. The test fake records effects, queues
  command and UPS results, and injects operation failures without touching the
  OS or port 3493.
- Ship `nut doctor` with a 40-check registry, JSON, `--strict`/`--observe`/`--list`,
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
