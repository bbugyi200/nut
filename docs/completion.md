# Shell completion

Install for the shell you use:

```sh
nut completion install zsh
nut completion install bash
nut completion status
nut completion status --json
nut completion uninstall
```

## What it looks like

`nut <Tab>` in zsh at 80 columns (see the generated
[`completion.txt`](completion.txt)):

```text
Monitor
  status      Show UPS status
  battery     Show battery charge and runtime, or run a guarded self-test
  events      Show UPS event history
Act
  beeper      Show or change the UPS beeper
  notify      Send or inspect UPS notifications
Set up
  init        Plan or apply NUT configuration
  rollback    List or restore a configuration backup
  config      Inspect, scaffold, render, or diff host settings
Prove
  doctor      Check UPS protection and recovery readiness
  bios        Guide and verify BIOS recovery settings
  drill       Guide and record a physical outage drill
Shell
  completion  Install, inspect, or remove shell completion
```

Headings sit flush left. Candidates are indented two spaces. Every
description in the menu starts in one shared column. Headings are bold cyan,
names are bold, descriptions are dim. `NO_COLOR` turns color off.

## How Tab decides what to offer

At a command slot it lists commands. At a value slot it lists values. Options
appear when you type `-`, or at a leaf where nothing else can follow.

- A value slot first asks its provider (UPS names, doctor checks, rollback
  stamps, board profiles, `--since`/`--boot` suggestions, shells), then clap
  `PossibleValue`s, then path hints (`!files`/`!dirs`), and otherwise shows a
  styled hint such as `PORT  Loopback upsd port …`.
- A live lookup that comes back empty says what it tried: `status --ups` with
  upsd offline shows `UPS  upsd at 127.0.0.1:3493 did not answer within 150 ms`.
- Flags already present (unless repeatable), flags conflicting with what is
  typed, hidden args, and the refused `drill --yes` / `bios setup --yes` never
  appear.
- No menu line wraps: descriptions truncate with `…` (`...` outside UTF-8
  locales). In very narrow panes, descriptions drop and only names show.
- In bash, `nut <Tab><Tab>` shows `status      Show UPS status` lines, one per
  row, in menu order (`compopt -o nosort`). There are no headings or colors,
  which readline cannot show.

## Customizing with zstyle

Aim styles at `':completion:*:*:nut:*'` or at one `nut-<group>` tag
(`nut-monitor`, `nut-options`, `nut-global-options`, `nut-color-modes`,
`nut-shells`, `nut-ups`, `nut-sections`, `nut-checks-<slug>`, `nut-backups`,
`nut-board-profiles`, `nut-since`, `nut-boots`). For example:

```zsh
zstyle ':completion:*:*:nut:*' list-colors '=*=35'
```

`NO_COLOR` (non-empty) turns nut's colors off. nut leaves no styles behind in
the user's shell; its scoped temporary `zstyle`s vanish after each Tab.

## Protocol and extension points

The hidden endpoint is independent of normal command execution:

```text
nut __complete <zsh|bash> --protocol 2 [--suffix TEXT] [--columns N] -- WORD...
```

Words are unquoted shell arguments through the cursor, including the program name
and an empty final word after a space. The final word contains only text before
the cursor. Suffix text is accepted for cursor-in-word completion; shells retain
it. A malformed `--columns` exits 2 like the other flags.

Response (v2) is UTF-8 lines on stdout, exit 0. There are three kinds of line:

- Directives: `!prefix N`, `!files [GLOB]`, `!dirs`, and `!message TEXT`, plus
  `!hint LABEL<TAB>TEXT` and `!group TAG<TAB>HEADING` (which starts the group
  later candidates belong to).
- Candidates: `VALUE<TAB>DISPLAY<TAB>space|nospace`. Rust always emits a
  `!group` before the first candidate.

Adapters ignore unknown directives and extra trailing fields. Values that are
empty, contain controls, or start with `!` are dropped. Protocol skew gives a
reinstall hint; malformed requests exit 2. Operational failures produce no
candidates and exit 0. A v1 request gets a v1 response built from the same menu
(`value<TAB>description<TAB>heading<TAB>space`), so the previously installed
adapter keeps working until reinstalled. Candidates are never evaluated as
shell code. No hidden request writes files, prompts, or sends a UPS control
command.

Rust decides what the menu contains and how it is laid out. The shell adapters
only hand it to the shell. The `Provider` trait receives a command path,
argument ID, and original words. Returning `Some` owns the slot with headed
groups or a hint; `None` falls back to clap.

Restart zsh (`exec zsh`) to load a new adapter; compinit does not reload an
already loaded function.

Protocol and shell adapters are adapted from the MIT-licensed
`bobs-org/bob-cli` completion implementation, copyright 2026 Bryan M Bugyi.

`cargo test --locked --test completion` requires bash and zsh. CI runs it along
with `just install-smoke`, which copies a binary into a temporary root, traverses
every visible command's help, runs a read-only invocation of every command group,
then installs, probes, and removes both adapters under a sandboxed HOME.

## Related

- [Getting started](getting-started.md)
- [CLI reference](reference/cli/completion.md)
- [Development](development.md)
