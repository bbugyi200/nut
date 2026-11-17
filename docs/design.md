# Design language

`nut` should feel calm, exact, and quick to read: one screen answers "am I protected?"
These rules are the spec. Snapshot tests cover the screens.

## Palette and glyphs

Colors carry meaning only: green for ok, amber for warn, red for fail, cyan for info and
headings, and dim for secondary text. Accents are bold. Colors come from `ui::theme`.

Glyphs are `✓ ⚠ ✗ ○ ▸ ⚡ →`, with ASCII fallbacks `ok !! xx -- > * ->` when the locale
is not UTF-8, when `TERM=dumb`, or with `--ascii`.

`--color auto|always|never` honors `NO_COLOR` and `CLICOLOR_FORCE`. Output to a pipe
never contains ANSI.

## Numbers, flags, and layout

Durations look like `25m 50s`, `1h 02m`, or `4s`. Power looks like `≈ 280 W of 1000 W`.
Times use local time; dates within the past week are relative ("3 days ago") with the
absolute date in `--verbose` and JSON.

Status flags become sentences. "On battery, discharging" or "On line, charging", never
raw `OB DISCHRG` except under `--all`.

Width is read from `COLUMNS` or the terminal, clamped between 60 and 120 columns.
Screens use aligned label columns, a single title line, and no box borders, so output
copies and pastes cleanly. Bars use eighth-block characters (`#`/`.` in ASCII).

Voice is short sentences. Every ⚠/✗ line is followed by `→ <exact command>`. No
exclamation marks and no emoji beyond the glyph set.

`-j/--json` on every read command emits `{"schema_version": 1, …}`. Exit codes are 0 for
success (including doctor warnings), 1 for an operational failure, and 2 for usage.

## Reference mockups

Exact spacing is an implementation choice; the content and hierarchy are not.

```text
$ nut
 cp1500  CyberPower CP1500PFCLCD                          athena · 14:02:11
 ✓ On line power, fully charged

   Charge   ████████████████████  100%     Runtime  25m 50s
   Load     █████▌░░░░░░░░░░░░░░   28%     ≈ 280 W of 1000 W
   Input    121.0 V                        Beeper   enabled

 If the power fails now
   +0:30   "on battery" email
   +3:00   clean shutdown begins
   +4:00   outlets off, 21m 50s of runtime kept in reserve
   mains   outlets on → BIOS boots athena            ✓ proven by drill 2026-11-20

 Last outage  3 days ago · 2m 13s · no shutdown
```

```text
$ nut doctor
 nut doctor · athena · 2026-11-16 14:03
 Hardware
   ✓ hw.usb               one CyberPower 0764:0601, device node group nut
   ✓ hw.delays            shutdown 60s, start 0s (profile 60 / 0)
 Protection
   ⚠ protect.poweroff-wait  30m is under full-charge runtime + 10m (35m 50s)
                            → set shutdown.poweroff_wait = "45m", then: sudo nut init
 Proof
   ○ proof.outage         never drilled → sudo nut drill outage
 19 passed · 1 warning · 0 failed · 6 skipped (root-only: sudo nut doctor)
```

The committed demo capture in [`screenshot.txt`](screenshot.txt) is the live renderer
against the scripted fixture, regenerated with `just screenshot`.

## Completion

Tab offers the next word: commands at a command slot, values at a value slot,
options after `-` or at a leaf. One column, one hierarchy: headings flush left,
candidates indented two spaces, every description starting in one shared column,
matching `nut --help`. Every candidate explains itself; free-form slots show a
hint, never silence. The palette follows the rules above: headings bold cyan
(info), names bold (accent), descriptions dim (secondary). `NO_COLOR` turns
color off.

## Related

- [Architecture](architecture.md)
- [Outage timeline](outage-timeline.md)
- [CLI reference](reference/cli/index.md)
- [Getting started](getting-started.md)
