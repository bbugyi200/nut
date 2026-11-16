# Shell completion

Install for the shell you use:

```sh
nut completion install zsh
nut completion install bash
nut completion status
nut completion status --json
nut completion uninstall
```

Each Tab asks the `nut` on PATH for the current command tree. Zsh groups and
describes commands using the same Monitor, Act, Set up, Prove, and Shell headings
as `nut --help`. Bash completes the same values. `status --ups` and `battery --ups`
query the selected loopback upsd with `LIST UPS`; the entire query has a 150 ms
budget, including slow partial responses. `--upsd-address` and `--upsd-port`
preceding the cursor choose that connection. An offline server yields no names.
Board-profile IDs include installed overrides; rollback stamps come from
`/var/backups/nut-cli/`. An unreadable directory yields no stamps.

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

Only `completion::engine` imports the exactly pinned `clap_complete` dependency.
The `Provider` trait receives a command path, argument ID, and original words.
Returning `Some(values)` owns the slot; `None` falls back to clap. Later phases can
extend `LiveProviders` for doctor check IDs or evidence records without changing
the protocol or adapters. Drill names and other subcommands already come from clap.

Protocol and shell adapters are adapted from the MIT-licensed
`bobs-org/bob-cli` completion implementation, copyright 2026 Bryan M Bugyi.

`cargo test --locked --test completion` requires bash and zsh. CI runs it along
with `just install-smoke`, which copies a binary into a temporary root, traverses
every visible command's help, then installs, probes, and removes both adapters
under a sandboxed HOME.
