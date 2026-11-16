# nut

A Rust CLI to manage, check, and prove [Network UPS Tools](https://networkupstools.org/)
protection. Cargo package: `nut-cli`; executable: `nut`; library: `nut_cli`.

This repository currently contains the compiling bootstrap. `nut --help` and
`nut --version` work; monitoring and management commands will arrive in later phases.
It does not connect to a UPS or change system configuration.

```sh
just check
cargo run --locked -- --help
cargo run --locked -- --version
```

Rust is pinned in `rust-toolchain.toml`; rustup installs the required components.
See [development](docs/development.md) for the local gate and CI.

## Safety contract

NUT daemons, `upssched`, and a small shell hook own the outage path. Protection must
continue to work when this CLI is missing or broken. Reads are the default; writes
must be planned, diffed with secrets masked, backed up, and confirmed. Agents must
never run power drills, BIOS setup, or battery tests. Build and test recipes operate
offline and never contact a real UPS or another host implicitly.

See [the complete safety contract](docs/safety.md). Generic hardware profiles belong
here; machine-specific policy belongs in the host's configuration repository.

Licensed under [MIT](LICENSE).
