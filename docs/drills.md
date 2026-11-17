# Guided recovery drills

`nut drill` shows the latest proof for each physical drill. A new drill is started
with `sudo nut drill blip`, `sudo nut drill ac-loss`, `sudo nut drill cycle`, or
`sudo nut drill outage`. The commands guide the operator and record evidence; they
do not shut down the host, switch UPS outlets, or inject NUT events. The keyboard
steps shown for cycle and outage include commands that the operator runs manually.

## Interlocks

Before a drill starts, `nut` requires root, interactive stdin and stdout, and the
machine hostname typed back exactly. It also reads a live UPS snapshot and refuses
unless the UPS reports `OL` without `OB`, `LB`, or `FSD`, the battery is at least
95% charged, and `/var/log/journal` makes the system journal persistent. The CLI
refuses `--yes`. It displays the age of the most recent configuration backup, or
warns when it cannot find one.

Before showing the physical steps, `nut` writes
`/var/lib/nut-cli/drills/pending.json`. The record pins the BIOS version and EFI
variable fingerprint, NUT version, hash of the host and live NUT configuration,
and configured battery installation date. It also stores the initial UPS status,
charge, runtime, host, and boot ID. Secrets and raw configuration contents are not
stored in the record. Drill records share the protected state tree that `nut init`
manages: `/var/lib/nut-cli` and its `drills` directory are `root:root` 0700, and
each record is published atomically, without following links, as a `root:root`
0600 file.

## Drill steps

| Drill | Procedure |
| --- | --- |
| `blip` | Unplug mains for at least 60 seconds. The CLI watches for `OB` then `OL`, records runtime when it first sees `OB`, and captures journal notification events. |
| `ac-loss` | At the keyboard, power off the host, switch the PSU off for 30 seconds, then switch it back on. Do not press the case power button. |
| `cycle` | With mains present, run `sudo upsmon -c fsd` at the keyboard. Wait for the UPS outlets to turn off and return. Do not press the case power button. |
| `outage` | Unplug UPS mains, run `sudo upsmon -c fsd` at the keyboard, and wait for outlets to turn off. Restore mains and wait for the host to boot. Do not press the case power button. |

Test 5, the lamp-only battery drain, remains a manual check and is not run by this
CLI.

## Reporting and freshness

After the test (and after reboot for `ac-loss`, `cycle`, and `outage`), run
`sudo nut drill report`. The report reads the relevant boot's NUT journal and asks
the operator whether the outlet behavior matched, whether the host stayed on or
booted as expected, whether the case power button stayed untouched, and, for a
blip, whether both notifications arrived. These answers are stored as human
attestations. The report becomes a passing proof only when all required answers
are yes and the blip watch observed at least 60 seconds between `OB` and `OL`.

A pending report appears as a reminder in the regular `nut` status view and in
`nut drill`. Doctor lists `proof.blip`, `proof.ac-loss`, `proof.cycle`, and
`proof.outage`. A proof is stale after its BIOS version, EFI fingerprint, battery
installation date, NUT version, host config, or live NUT config changes. The
doctor fix command names the drill to repeat. Because the records are root-only, a
non-root `nut doctor` skips the proof checks and points to `sudo nut doctor`
instead of reporting them as unknown.

Drill records are evidence of what was observed and what the operator attested;
they do not prove that every future outage will recover successfully. Never run a
physical drill from an agent, build, CI job, or remote recipe.

## Related

- [Safety contract](safety.md)
- [BIOS guidance](bios.md)
- [Outage timeline](outage-timeline.md)
- [CLI reference](reference/cli/drill.md)

