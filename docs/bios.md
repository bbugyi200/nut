# BIOS guidance and attestation

`nut bios` matches a board profile from DMI `board_vendor` / `board_name` (never
`sys_vendor` / `product_name`), shows each required firmware setting, audits OS
boot-readiness, and records a receipt pinned to the BIOS version and an efivar
fingerprint.

On the ASUS PRIME TRX40-PRO there is no `/sys/class/firmware-attributes` write
path. The manual backend reports **attested** or **unknown**, never **verified**.
A later board with firmware-attributes can apply settings through sysfs; that
backend is fixture-tested until a real machine uses it.

## Commands

- `nut bios` — board, BIOS version, matched profile, setting state, boot
  readiness, and attestation freshness.
- `nut bios guide` — numbered keyboard walkthrough that is easy to read off a
  phone.
- `sudo nut bios setup` — TTY plus typed hostname; records a pending
  attestation; runs `systemctl reboot --firmware-setup` when firmware advertises
  it, otherwise prints `press Del at POST`. Refuses `--yes`.
- `sudo nut bios attest` — yes/no for each setting; writes
  `/var/lib/nut-cli/bios/attestation.json`.
- `nut bios profile list|show|new` — catalog, one profile, or a DMI-filled
  scaffold with TODOs.

Bare `nut` prints a reminder while an attestation is pending.

## Athena (PRIME TRX40-PRO)

1. Advanced → APM Configuration → Restore On AC Power Loss = **Power On** (never
   Last State).
2. ErP Ready = Disabled.
3. Boot → Wait For 'F1' If Error = Disabled (optional).
4. ASUS needs about 30 s of true AC loss before Power On fires. A UPS outlet
   cycle provides that.

A changed BIOS version or efivar fingerprint makes the receipt **stale**. That
is always a warning, never a failure: re-run `sudo nut bios attest`.

## Boot-readiness audit

The same checks are reused by `nut doctor`:

- EFI boot order starts with a Linux entry
- config-listed mounts have `nofail`
- `findmnt --verify` when readable
- journald is persistent
- advisory `kernel.panic` and watchdog hints

See [adding a board](adding-a-board.md) to extend the catalog.

## Related

- [Guided recovery drills](drills.md)
- [Doctor checks](doctor-checks.md)
- [CLI reference](reference/cli/bios.md)

