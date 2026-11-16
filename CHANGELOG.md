# Changelog

## Unreleased

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
