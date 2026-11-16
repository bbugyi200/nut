# nut

A Rust CLI to manage, check, and prove [Network UPS Tools](https://networkupstools.org/)
protection. Cargo package: nut-cli; executable: nut; library: nut_cli.

The current tree contains the grouped CLI tree, shared UI and error model, host
abstraction, the `schema = 1` host configuration and UPS profiles, typed
renderers for every NUT file plus the `nut-upssched-cmd` hook, and `nut bios`
(guide, setup, attest, and board profiles). Remaining commands are reserved for
later phases; they do not connect to a UPS or change NUT configuration yet.

~~~sh
just check
cargo run --locked -- --help
cargo run --locked -- --version
~~~

Rust is pinned in rust-toolchain.toml; rustup installs the required components.
See [development](docs/development.md) for the local gate and CI.

## Config commands

nut --help groups commands by workflow. The currently implemented commands are:

- `nut config path` prints the selected host settings path and why it was chosen.
- `nut config show` parses and summarizes resolved host settings; add `--json` for schema-versioned JSON.
- `nut config new [-o PATH]` scaffolds a commented host config from live USB and DMI discovery.
- `nut config render [-o DIR]` writes NUT files and the event hook with secret placeholders.
- `nut config diff` compares that rendering with live files, masking secrets. Non-root reads of `/etc/nut` are reported as `contents need root`.
- `nut bios` shows the matched board profile, setting state, boot-readiness audit, and attestation freshness.
- `nut bios guide` prints a numbered keyboard walkthrough.
- `nut bios profile list|show|new` lists, prints, or scaffolds a board profile.

See [BIOS guidance](docs/bios.md) and [adding a board](docs/adding-a-board.md).
`sudo nut bios setup` and `sudo nut bios attest` are interactive and must not be
run by agents.

Select a config with `-c/--config PATH`, then `NUT_CONFIG`, then `/etc/nut-cli/nut.toml`.
The shared UI honors `--color auto|always|never`, `NO_COLOR`, `CLICOLOR_FORCE`,
`--ascii`, and terminal width. See [configuration](docs/configuration.md) for the
schema, profiles, and generic example.

## Safety contract

NUT daemons, upssched, and a small shell hook own the outage path. Protection must
continue to work when this CLI is missing or broken. Reads are the default; writes
must be planned, diffed with secrets masked, backed up, and confirmed. Agents must
never run power drills, BIOS setup, or battery tests. Build and test recipes operate
offline and never contact a real UPS or another host implicitly.

See [the complete safety contract](docs/safety.md). Generic hardware profiles belong
here; machine-specific policy belongs in the host's configuration repository.

Licensed under [MIT](LICENSE).
