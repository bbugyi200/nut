# Host configuration

The host file is TOML with schema_version = 1. It describes machine policy and points
at reusable UPS and board profiles. Hardware profiles belong in the nut crate; a
machine's selected profile and policy values belong beside that machine's deployment
configuration.

A minimal file looks like this:

~~~toml
schema_version = 1

[host]
name = "example-host"
board_profile = "example-board"

[ups]
name = "main"
profile = "example-ups"
upsd_address = "127.0.0.1"
upsd_port = 3493

[protection]
poweroff_wait = "45m"
on_battery_shutdown = "180s"
low_battery_runtime = "5m"
battery_replaced = "2026-01-01"

[[mounts]]
path = "/srv/data"
nofail = true

[notifications]
recipients = ["operator@example.invalid"]
~~~

host.board_profile is optional. upsd_address and upsd_port default to loopback and
3493. Durations are integer seconds (s), minutes (m), or hours (h). Unknown fields are
rejected so misspelled or unsupported settings do not silently disappear. The schema
has no credential fields: NUT secrets stay in protected runtime files.

Use nut config path to see which file the CLI selected and nut config show to check
that it parses. --config PATH overrides NUT_CONFIG, which overrides the XDG default.
