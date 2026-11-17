# Shell completion

Install for the shell you use:

```sh
nut completion install zsh
nut completion install bash
nut completion status
nut completion status --json
nut completion uninstall
```

Each Tab asks the `nut` on your PATH for the current command tree. After
reinstalling an adapter, run `exec zsh` (or open a new shell) so the loaded
function is replaced.

## What it looks like

Zsh draws grouped menus with lowercase `── name ──` headers in bold cyan,
merged `--long  -s` option rows, and a description on every row. The root
menu is commands only, in the same order as `nut --help`:

```text
── monitor ──
status   -- Show UPS status
battery  -- Show battery charge and runtime, or run a guarded self-test
events   -- Show UPS event history
── act ──
beeper  -- Show or change the UPS beeper
notify  -- Send or inspect UPS notifications
── set up ──
init      -- Plan or apply NUT configuration
rollback  -- List or restore a configuration backup
config    -- Inspect, scaffold, render, or diff host settings
── prove ──
doctor  -- Check UPS protection and recovery readiness
bios    -- Guide and verify BIOS recovery settings
drill   -- Guide and record a physical outage drill
── shell ──
completion  -- Install, inspect, or remove shell completion
```

A `-` word offers short and long forms together (`--help  -h`). Below the
root, the command's own flags sit in `options` and propagated globals sit in
`global options`. `NO_COLOR` keeps the same headers without color. When
`TERM=dumb` or the locale is not UTF-8, headers fall back to `== name ==`.
Bash completes the same values as a plain list; readline cannot draw headers.

## Presentation rules

1. The root empty word offers commands only, grouped by the same Monitor,
   Act, Set up, Prove, and Shell headings as `nut --help`, lowercased.
2. Below the root, an empty word offers subcommands when the command has
   any. Otherwise it offers the next positional's values when that slot has
   a value source, and options only when nothing else applies.
3. If that would offer exactly one subcommand of a command that also runs
   bare (`nut battery`, `nut notify`), the command's options are offered as
   well so zsh shows a menu instead of inserting a side-effecting `test`.
4. A `-` word offers short and long forms adjacent with identical
   descriptions. A `--…` word offers long forms only. Option groups are
   sorted alphabetically so merged `-` rows and `--` rows keep the same
   order.
5. Value slots always win: `--ups <Tab>` completes UPS names, including a
   cursor that starts with `-` for `allow_hyphen_values` options such as
   `--since`. `--opt=<Tab>` and short attached forms keep their prefix.
6. Already-typed options, conflicting options, hidden arguments, `--help`
   after other arguments, and the refused flags `drill --yes` and
   `bios setup --yes` are not offered. `doctor` does not re-offer check IDs
   or section slugs already typed.
7. Every row has a description. Empty live lookups and free-text slots emit
   a one-line hint instead of a silent empty menu.

`status --ups` and `battery --ups` query the selected loopback upsd with
`LIST UPS`; the entire query has a 150 ms budget, including slow partial
responses. `--upsd-address` and `--upsd-port` preceding the cursor choose
that connection. An offline server yields `upsd at ADDR:PORT did not
answer`. Board-profile IDs include installed overrides; rollback stamps
come from `/var/backups/nut-cli/`. An unreadable directory yields a root
hint; a missing directory yields `no configuration backups yet`.

Installation selects `$SHELL` if the shell argument is omitted. Zsh uses the first
existing user-owned fpath directory under HOME, otherwise `~/.zfunc`. Ensure that
directory is in fpath **before** compinit in `~/.zshrc`, for example:

```zsh
fpath=(~/.zfunc $fpath)
autoload -Uz compinit
compinit
```

Bash uses `${BASH_COMPLETION_USER_DIR}/completions/nut` if set, otherwise
`${XDG_DATA_HOME:-~/.local/share}/bash-completion/completions/nut`. Enable the
bash-completion package in your shell, or source the printed adapter path from
`~/.bashrc`. Installation does not edit startup files. Restart your shell after
installing or removing adapters; an already loaded function remains in memory.

Use `--target DIR` for a custom directory, `--dry-run` to preview, and `--force`
to replace a foreign or modified file. Writes use same-directory temporary files,
fsync, and atomic rename. A versioned manifest under
`${XDG_STATE_HOME:-~/.local/state}/nut-cli/completion.json` records paths and SHA-256
hashes. Reinstalling unchanged adapters preserves their mtimes. Uninstall checks
every selected file before removing any, and refuses modified files. Uninstall
before moving a tracked adapter to another directory.

Install and status probe registration in **isolated real shells**: zsh compinit
with the target on fpath, and bash `complete -p nut` after sourcing the adapter.
`registered: true` confirms that the adapter loads; it does not claim that an
already running interactive shell has loaded it. Status distinguishes missing,
modified, outdated, current, and uninstalled adapters. It never repairs them.

`nut completion zsh` and `nut completion bash` print the embedded adapters for
manual installation. They do not require HOME or a manifest.

## Protocol and extension points

The hidden endpoint is independent of normal command execution:

```text
nut __complete <zsh|bash> --protocol 1 [--suffix TEXT] -- WORD...
```

Words are unquoted shell arguments through the cursor, including the program name
and an empty final word after a space. The final word contains only text before
the cursor. Suffix text is accepted for cursor-in-word completion; shells retain
it. Output is directives followed by tab-separated
`value`, `description`, `group`, and `space|nospace` fields. Directives are
`!prefix N` (preserve N Unicode characters), `!files`, `!dirs`, and `!message TEXT`.
Unknown directives are ignored. Protocol skew gives a reinstall hint; malformed
requests exit 2. Operational failures produce no candidates and exit 0. Candidates
are never evaluated as shell code. No hidden request writes files, prompts, or
sends a UPS control command.

The presenter walks clap's public `Command`/`Arg` API. The `Provider` trait
returns `Some(Slot)` to own a value slot (`Candidates` or `Message`); `None`
falls back to static tables, clap possible values, path hints, and a
free-text message. Providers stay read-only and bounded. Later phases can
extend `LiveProviders` without changing the protocol or adapters.

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
