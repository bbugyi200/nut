# Host configuration

The host file is TOML with `schema = 1`. It describes machine policy and points at
reusable UPS and board profiles. Hardware profiles belong in the nut crate; a machine's
selected profile and policy values belong beside that machine's deployment
configuration. v1 supports exactly one UPS.

A minimal file looks like this:

~~~toml
schema = 1

[host]
name = "example-host"

[ups.primary]
profile = "usbhid-generic"
description = "Example USB HID UPS"
port = "auto"
offdelay = "60s"
ondelay = "0s"

[ups.primary.match]
vendorid = "0764"
productid = "0601"

[ups.primary.extra]
maxretry = 3
retrydelay = 5

[shutdown]
notify_after = "30s"
early_after = "3m"
poweroff_wait = "45m"
shutdown_budget = "2m"

[notify]
from = "root@example.invalid"
to = "operator@example.invalid"
commbad_after = "60s"

[boot]
nofail_mounts = ["/srv/data"]
board_profile = "asus-prime-trx40-pro"

[operators]
users = ["operator"]
journal = false
~~~

`nut config path` prints the selected file and why it was chosen (`--config`,
`NUT_CONFIG`, or the default `/etc/nut-cli/nut.toml`). `nut config show` prints the
resolved values with each hardware field's source (host, profile, or default).
`nut config new` scaffolds a commented file from live USB and DMI discovery.
`nut config render` writes NUT files and the `nut-upssched-cmd` hook with secret
placeholders. `nut config diff` compares that rendering with live files and masks
passwords; unreadable `/etc/nut` files are reported as `contents need root`.

Durations are an integer followed by `s`, `m`, or `h`. Unknown fields are rejected so
misspelled or unsupported settings do not silently disappear. The schema has no
credential fields: NUT secrets stay in protected runtime files. UPS names, mail
addresses, and `extra` ups.conf values are validated before interpolation so a stray
quote or newline cannot inject an `AT … EXECUTE` line or shell.

Init installs the validated source config as root-owned `/etc/nut-cli/nut.toml`
with mode `0644`. The generated operator password is stored separately as one
password followed by a newline in `/etc/nut-cli/operator.secret`, owned by
`root:nut-operators` with mode `0640`. Do not put this password in host TOML,
command arguments, environment variables, or logs. `nut doctor` checks these
permissions and the operator's exact command allowlist. Rollback restores the
previous file bytes and metadata while retaining installed distribution
packages; see [init rollback](init-rollback.md).

Embedded UPS profiles live in `profiles/ups/`. Host-local overrides may be dropped in
`/etc/nut-cli/ups/*.toml` and `/etc/nut-cli/boards/*.toml` without rebuilding nut.
The CyberPower CP1500PFCLCD profile pins delays 60/0, USB ids `0764:0501` and
`0764:0601`, 1000 W, and the RB1290X2 / RB1280X2B cartridges. Athena's unit is 0601,
so its host file sets `match.productid`.
