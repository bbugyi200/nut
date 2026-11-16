# nut

A Rust CLI to manage, check, and prove [Network UPS Tools](https://networkupstools.org/)
protection. Cargo package: nut-cli; executable: nut; library: nut_cli.

The current foundation contains the grouped CLI tree, shared UI and error model,
host abstraction, typed host configuration and hardware profile models, and
nut config path|show. Commands outside config are reserved for their later
implementation phases; they do not connect to a UPS or change system configuration yet.

~~~sh
just check
cargo run --locked -- --help
cargo run --locked -- --version
~~~

Rust is pinned in rust-toolchain.toml; rustup installs the required components.
See [development](docs/development.md) for the local gate and CI.

## Foundation commands

nut --help groups commands by workflow. The currently implemented commands are:

- nut config path prints the selected host settings path.
- nut config show parses and summarizes host settings; add --json for schema-versioned JSON.

Select a config with --config PATH, then NUT_CONFIG, then
$XDG_CONFIG_HOME/nut/nut-cli.toml, or $HOME/.config/nut/nut-cli.toml.
The shared UI honors --color auto|always|never, NO_COLOR, CLICOLOR_FORCE,
--ascii, and terminal width. See [configuration](docs/configuration.md) for the
initial schema and generic example.

## Safety contract

NUT daemons, upssched, and a small shell hook own the outage path. Protection must
continue to work when this CLI is missing or broken. Reads are the default; writes
must be planned, diffed with secrets masked, backed up, and confirmed. Agents must
never run power drills, BIOS setup, or battery tests. Build and test recipes operate
offline and never contact a real UPS or another host implicitly.

See [the complete safety contract](docs/safety.md). Generic hardware profiles belong
here; machine-specific policy belongs in the host's configuration repository.

Licensed under [MIT](LICENSE).
