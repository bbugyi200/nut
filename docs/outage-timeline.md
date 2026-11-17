# Outage timeline

`nut status` prints what happens if power fails now. Those times come from the host
config when present; otherwise NUT's default low-battery timeline, labeled as such.

## Host timers

| Setting | Meaning |
| --- | --- |
| `shutdown.notify_after` | Delay after on-battery before the hook sends mail. Shorter observed outages are **blips**. |
| `shutdown.early_after` | Delay after on-battery before the hook runs `upsmon -c fsd`. |
| `shutdown.shutdown_budget` | Time reserved for a clean OS shutdown after FSD. |
| `shutdown.poweroff_wait` | How long the UPS keeps outlets off after shutdown so the board sees true AC loss. |

The early timer plus `shutdown_budget` plus the UPS off delay plus about 60 s must fit
in the runtime at ≥95% charge. `POWEROFF_WAIT` must be at least full-charge runtime plus
10 minutes so a reboot net cannot fire while the UPS is still on battery.
`nut doctor` checks both. See [doctor checks](doctor-checks.md).

On battery, status shows a countdown to the early timer when the on-battery start is
known from `--watch` or the journal. Otherwise it says the start is unknown.

## What the protection path does

The rendered POSIX hook owns the events. Early shutdown execs `upsmon -c fsd`. Other
events send bounded mail and log under `nut-upssched`. The UPS then turns outlets off
for `POWEROFF_WAIT`, turns them back on, and the BIOS Power On setting boots the host.
`nut` is not in that path. See [architecture](architecture.md).

## Related

- [Design language](design.md)
- [Journal events](events.md)
- [Guided recovery drills](drills.md)
- [Host configuration](configuration.md)
