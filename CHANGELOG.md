# Changelog

## Unreleased

- Add DMI-matched board profiles (`asus-prime-trx40-pro` and `generic`), efivar
  fingerprints, a `BiosBackend` trait with manual and firmware-attributes backends,
  and the OS boot-readiness audit.
- Ship `nut bios` with `guide`, `setup`, `attest`, and `profile list|show|new`.
- Remind on bare `nut` while a BIOS attestation is pending.
- Add `just remote` for read-only commands on a named host.
- Bootstrap the `nut-cli` Rust package with the `nut` executable and `nut_cli` library.
- Add help/version output, offline CLI tests, and the documented safety contract.
- Pin Rust, commit the dependency lockfile, and add the Justfile and SHA-pinned CI gates.
- Register the SASE `check` tool with gate inputs and toolchain fingerprints.
