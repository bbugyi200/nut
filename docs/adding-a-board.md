# Adding a board profile

A motherboard is one TOML file. Embedded profiles live in
`profiles/boards/*.toml`. Files in `/etc/nut-cli/boards/*.toml` override them by
`id` so a new machine can work without a rebuild.

## Schema

```toml
schema = 1
id = "vendor-board-slug"
backend = "manual" # or "firmware-attributes"
fingerprint_efivars = ["Setup-*", "AmdSetup-*"]
caveats = ["Vendor-specific AC-loss quirks."]
sources = ["Board manual section."]

[match]
board_vendor = "ASUSTeK COMPUTER INC."
board_name = "PRIME TRX40-PRO"

[keys]
setup = "Del"
advanced_mode = "F7"
save = "F10"

[[settings]]
id = "ac-power-loss"
menu_path = ["Advanced", "APM Configuration", "Restore On AC Power Loss"]
required = "Power On"
why = "One line: why this value is required for unattended power-on."
# optional = true
# attribute = "AcPwrRcvry"  # firmware-attributes sysfs name
```

Match only DMI `board_vendor` and `board_name`. Placeholders such as
`sys_vendor` / `System Product Name` are ignored on purpose.

`nut bios profile new` prints a scaffold filled from the live DMI. Fill the
TODOs, save it as `profiles/boards/<id>.toml` or under `/etc/nut-cli/boards/`,
and confirm `nut bios` matches it.

Unknown boards use the embedded `generic` profile: common labels (AC Power
Recovery, AC Back, After Power Failure, State After G3, Restore On AC Power
Loss) and the rule **Power On, never Last State**.

## Backends

| Backend | Read | Write |
| --- | --- | --- |
| `manual` | attested / unknown from the receipt | keyboard, via `nut bios setup` |
| `firmware-attributes` | `/sys/class/firmware-attributes/*/attributes/<name>/current_value` | plan, confirm, root; honors `pending_reboot` |

The next backend shape is IPMI/Redfish chassis power-restore policy
(`always-on` / equivalent). Do not stub it here: add a real backend when a
machine exposes that interface.

Firmware-attributes apply is fixture-tested against a fake sysfs tree until a
real Dell/Lenovo/HP board uses it.

## Related

- [BIOS guidance](bios.md)
- [Host configuration](configuration.md)
- [Architecture](architecture.md)

