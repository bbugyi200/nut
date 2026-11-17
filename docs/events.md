# Journal events and outage stories

`nut events` reads accessible local system and user journals without changing NUT or the UPS.
It includes `upsmon`, `upssched`, `nut-upssched`, and the legacy `athena-upssched`
tag, so adopting the new hook does not discard older history.
User journals matter when an operator runs `nut notify test`: journald can store
that hook's messages separately from the NUT services' system messages. History,
follow, and the initial follow cursor use the same journal scope. The separate
system-access check still refuses a partial view from an unprivileged account.

```sh
nut events                         # latest 20 stories
nut events --since today --outages
nut events --since '-2h' --boot 0
nut events --boot -1 --raw --limit 100
nut events --follow
nut events --follow --json         # one JSON document per update
nut events --verbose               # also driver, server, and monitor units
```

Short options are `-s` (since), `-b` (boot), `-f` (follow), `-o` (outages),
`-r` (raw), `-n` (limit), `-j` (JSON), and `-v` (verbose). `--last-outage` is
an alias for `--outages --limit 1`. `--raw` and `--outages` are mutually exclusive.
`--since` accepts journalctl's time syntax, including `today`, `-2h`, and an
absolute date. Boot offsets follow journalctl: 0 is current, -1 is previous.

## What a story proves

- **Outage:** an observed ONBATT followed by ONLINE. The duration measures those
  journal timestamps. A completed outage shorter than `shutdown.notify_after`
  is a **blip**. Without a host config, the threshold is 30 seconds.
- **Shutdown:** FSD or SHUTDOWN evidence. When available, boot history adds the
  interval from the first shutdown record to the next boot's first retained
  journal entry. This does not prove that outlets switched or that BIOS booted
  the machine without human help.
- **Communication:** lost and restored messages are paired independently of
  outages, so interleaving them cannot consume power events.
- **Replace battery:** a replacement warning remains visible as its own story.
  Other selected messages remain as journal stories, including unfamiliar NUT
  messages. Every retained entry belongs to one story.

A delayed `onbatt` hook entry alone does not establish the outage's start. An
ONLINE without a start says **journal starts mid-outage**; an unpaired ONBATT
says **still on battery**, unless a later boot proves the observation ended.
Missing shutdown/recovery evidence stays unknown. `--since`, `--boot`, journal
rotation, and retention limits can all leave partial stories.

The existing hooks log **before** invoking sendmail. Their message proves only
that notification was **attempted**. An explicit mail failure is shown as
**failed**; explicit successful submission is **submitted**, not delivered.
Absent mail evidence is **unknown**. Charge is reported only when a source entry
carries `BATTERY_CHARGE`; ordinary upsmon logs and the existing hooks do not
record it. No historical charge or mail delivery is inferred from today's UPS
reading.

## Access and limits

When the system journal is inaccessible, the command explains why and prints
`sudo nut events`, including in its versioned JSON error document (exit 1).
Optional membership in `systemd-journal` grants access to **all system logs**;
choosing it is an administrative decision. This command never changes groups or
runs sudo. A readable journal with no matching entries prints that fact.

Queries retain at most 2,000 entries and 4 MiB of source JSON; omitted history
is explicitly marked `truncated`. Individual process lines are limited to
64 KiB, stderr to 16 KiB, and boot history to 1 MiB/1,000 boots. Finite reads time
out after five seconds each. Oversized or malformed input fails clearly rather
than returning a fabricated story. The default 20-story display limit is applied
**after pairing**, so it does not clip a story's beginning.

`--follow` captures a cursor before the initial query and replays from it,
deduplicating entries already in the initial view. It flushes each changed story
as it arrives, including a later completion of a pending outage. Stop with Ctrl-C.
Memory stays bounded by the same retained history limits. In verbose mode,
snapshot tag and unit queries are merged; follow reads and filters the journal
stream locally because journalctl intersects tag and unit filters.

JSON documents have `schema_version: 1`, an access state, truncation and boot
availability flags, and `stories` (or original journal objects in `entries` with
`--raw`). Stories include absolute Unix microsecond timestamps, duration,
shutdown and boot evidence, charge, mail evidence, and their source entries.
Follow emits JSON Lines, with `update: true` on subsequent documents. Story
updates retain the cursor of their first source entry as their identity.
Human output uses local times, relative times within the last week, absolute
times under `--verbose`, and the shared width/color/ASCII settings.

## Status integration

For a local UPS, `nut status` adds **Last outage** when journal evidence is
available, plus the observed start of the current on-battery period. The current
upsd reading must also say OB before a start time is treated as current. If live
power is online but the journal lacks ONLINE, the summary says so. Inaccessible
history is omitted quietly from status. JSON carries `last_outage` and
`on_battery_since_us` when available. Reads through a nondefault upsd port omit
local journal history, since a tunnel can point to another machine.

To inspect another host's history, run `nut events` on that host. During
development, `just remote HOST events` builds and copies the binary to the named
host's user cache and runs the same read-only command there. It never installs
it as root or changes NUT configuration.

Journal query semantics follow the [systemd journalctl manual](https://www.freedesktop.org/software/systemd/man/latest/journalctl.html).

## Related

- [Outage timeline](outage-timeline.md)
- [Guided recovery drills](drills.md)
- [CLI reference](reference/cli/events.md)

