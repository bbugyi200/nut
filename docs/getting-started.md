# Getting started

Install [rustup](https://rustup.rs/) and [just](https://github.com/casey/just), clone
this repository, and run the offline gate:

```sh
just check
just demo
just install-smoke
```

`just demo` starts a loopback fake upsd and prints `nut status` against the committed
fixture. `just install-smoke` copies a debug binary into a temporary root, walks every
visible command's `--help`, runs a read-only invocation of every command group, and
installs then removes both shell completion adapters.

## Install on a new machine

1. Build the release binary, install it root-owned at `/usr/local/bin/nut`, install the
   man pages, and install completion as the login user:

   ```sh
   just install
   ```

   That recipe calls `sudo`. Agents must submit the same `install` argv through
   `/sase_sudo` instead of invoking `sudo` themselves.

2. Scaffold a host config from live USB and DMI discovery:

   ```sh
   nut config new -o nut.toml
   ```

   Hardware profiles live in this repo. Machine policy (timers, mail, operators, battery
   date) belongs in that host file. See [configuration](configuration.md).

3. Preview the converge plan. `--dry-run` never elevates:

   ```sh
   nut --config ./nut.toml init --dry-run
   nut --config ./nut.toml init --dry-run --json
   ```

4. Apply through the reviewed plan hash. Interactive terminals confirm and re-exec
   `sudo -- /usr/local/bin/nut init --yes --expect-plan HASH --config ABS`. Agents take
   the hash from JSON and submit that argv through `/sase_sudo`. See
   [init and rollback](init.md).

5. Check protection:

   ```sh
   nut doctor
   sudo nut doctor
   ```

6. At the keyboard, not from an agent: `nut bios guide`, `sudo nut bios setup`,
   `sudo nut bios attest`, then `sudo nut drill ac-loss|blip|cycle|outage` and
   `sudo nut drill report`. See [BIOS guidance](bios.md) and [drills](drills.md).

Tagged GitHub Releases attach a static `x86_64-unknown-linux-musl` binary and its
SHA-256 checksum. Cut `v0.1.0` only after athena adoption.

## Related

- [Design language](design.md)
- [Architecture](architecture.md)
- [Safety contract](safety.md)
- [CLI reference](reference/cli/index.md)
- [Development](development.md)
