# nut

A Rust CLI to manage, check, and prove [Network UPS Tools](https://networkupstools.org/)
protection. Cargo package: nut-cli; executable: nut; library: nut_cli.

The current tree contains the grouped CLI, shared UI and error model, the
`schema = 1` host configuration and UPS profiles, typed renderers for every NUT
file plus the `nut-upssched-cmd` hook, `nut config path/show/new/render/diff`,
read-only `nut status` and `nut battery` commands backed by a bounded RFC 9271
client that accepts loopback upsd connections only, `nut bios` (guide, setup,
attest, and board profiles), and guided `nut drill` procedures with interlocks,
cross-boot attestations, and proof freshness checks. `nut config render` can write
generated files to a selected output directory; it does not apply them to live NUT
configuration. Journal history is available through `nut events`, including
outage stories and continuous legacy hook history. `nut doctor` reports protection
and recovery readiness with stable check IDs. `nut init` and `nut rollback`
converge and restore NUT configuration through a reviewed recoverable
transaction; isolated Ubuntu 24.04 CI proves that path against dummy-ups.

~~~sh
just check
cargo run --locked -- --help
cargo run --locked -- --version
~~~

Rust is pinned in rust-toolchain.toml; rustup installs the required components.
See [development](docs/development.md) for the local gate and CI.

## Implemented commands

`nut --help` groups commands by workflow. The currently implemented commands are:

- `nut config path` prints the selected host settings path and why it was chosen.
- `nut config show` parses and summarizes resolved host settings; add `--json` for schema-versioned JSON.
- `nut config new [-o PATH]` scaffolds a commented host config from live USB and DMI discovery.
- `nut config render [-o DIR]` renders NUT files and the event hook with secret placeholders; `-o DIR` writes them there.
- `nut config diff` compares that rendering with live files, masking secrets. Non-root reads of `/etc/nut` are reported as `contents need root`.
- `nut status` reads one UPS snapshot; use `--watch`, `--all`, or `--json` as needed.
- `nut battery` summarizes charge and estimated runtime.
- `nut beeper [mute|enable|disable]` shows or changes the beeper on the least-privilege operator credential; verbs confirm the new state.
- `nut battery test [--stop]` runs or aborts a guarded quick self-test; it needs an interactive terminal and a `[y/N]` confirmation, and agents cannot run it.
- `nut notify [test]` shows notification recipients and recent hook events, or sends a test notification through the installed hook. See [authenticated actions](docs/actions.md).
- `nut events` pairs journal entries into outage, blip, shutdown, and communication stories; use `--follow`, `--raw`, `--outages`, or `--json`.
  See [journal events](docs/events.md) for access guidance and evidence limits.
- `nut bios` shows the matched board profile, setting state, boot-readiness audit, and attestation freshness.
- `nut bios guide` prints a numbered keyboard walkthrough.
- `nut bios profile list|show|new` lists, prints, or scaffolds a board profile.
- `nut doctor [CHECK]...` reports protection checks with evidence and a `→` fix command; `--list` prints the catalog, `--observe SECS` watches driver restarts, and `--strict` fails on warnings. See [doctor checks](docs/doctor-checks.md). Doctor never fixes anything.
- `nut init [-n/--dry-run] [--rotate-secrets] [--expect-plan HASH] [-y/--yes]` previews the deterministic convergence plan; interactive runs confirm before applying, non-root elevation uses the reviewed `sudo -- /usr/local/bin/nut` argv, and `--dry-run`/`--json` never mutate or elevate. See [init and rollback](docs/init.md).
- `nut rollback [STAMP] [-l/--list] [-n/--dry-run] [-y/--yes]` lists backups or restores the selected transaction with the shared writer lock and safe-online gates; list and dry-run never mutate or elevate. Distribution packages stay installed.
- `nut drill` shows the four recovery proofs and their freshness.
- `sudo nut drill blip|ac-loss|cycle|outage` guides an interactive, guarded physical drill; `sudo nut drill report` records what happened after it. See [drill guidance](docs/drills.md).

See [BIOS guidance](docs/bios.md), [drill guidance](docs/drills.md), and [adding a board](docs/adding-a-board.md).
`sudo nut bios setup` and `sudo nut bios attest` are interactive and must not be
run by agents.

Select a config with `-c/--config PATH`, then `NUT_CONFIG`, then `/etc/nut-cli/nut.toml`.
The shared UI honors `--color auto|always|never`, `NO_COLOR`, `CLICOLOR_FORCE`,
`--ascii`, and terminal width. See [configuration](docs/configuration.md) for the
schema, profiles, and generic example.
Use `--ups`, `--upsd-address`, and `--upsd-port` to override the UPS name or
loopback endpoint for a single read.

Try the scripted demo without UPS hardware:

~~~sh
just demo
~~~

The status and fixture recipes require the target host explicitly and create a
temporary SSH tunnel to its loopback upsd. They do not run commands or write files
on that host:

~~~sh
just remote-status athena
just remote-battery athena
just fixture athena examples/fixtures/athena-status.json
~~~

Set `NUT_CONFIG` to a local host TOML file when remote status should also show the
machine-specific shutdown timer and host name.

## Safety contract

NUT daemons, upssched, and a small shell hook own the outage path. Protection must
continue to work when this CLI is missing or broken. Reads are the default; writes
must be planned, diffed with secrets masked, backed up, and confirmed. Agents must
never run power drills, BIOS setup, or battery tests. Build and test recipes operate
offline and never contact a real UPS or another host implicitly. Real NUT
integration lives in the disposable `just e2e` job on Ubuntu 24.04 with NUT
2.8.1 and a local dummy-ups `.dev` source.

See [the complete safety contract](docs/safety.md). Generic hardware profiles belong
here; machine-specific policy belongs in the host's configuration repository. The
captured fixture contains only the selected status fields, not every UPS variable.

Licensed under [MIT](LICENSE).

Shell completion: see [installation, status, and protocol](docs/completion.md).
