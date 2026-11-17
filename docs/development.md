# Development

Install rustup and just, then run `just check`. The pinned Rust 1.100.0 toolchain
includes rustfmt and clippy. `Cargo.lock` is committed. Build, lint, and test commands
use `--locked`. `cargo fmt` does not accept that flag; the formatting recipes first
validate the lockfile with `cargo metadata --locked --no-deps`.

The canonical gate checks formatting, the bob-cli compiler and Clippy deny lists,
forbidden process environment mutation, integration tests, doctests, and shellcheck
on the golden rendered event hook when `shellcheck` is installed. The lists were
copied verbatim from bob-cli's justfile, with `clippy::disallowed_methods` added
to enforce this project's `clippy.toml`.

- `just` lists recipes; `just all` runs the same gate as `just check`.
- `just format` applies Rust formatting; `just fmt` checks it.
- `just lint` runs Clippy on all targets and features.
- `just test` runs tests and doctests without stopping after the first failed binary.
- `just run *args` runs the crate's `nut` binary.
- `just remote host *args` copies a release binary to `HOST` over SSH and runs a
  read-only command (`bios`, `bios guide`, `bios profile`, `config path/show/diff`,
  `status`, `doctor`, and the other allowlisted verbs). It refuses `config new/render`,
  `bios setup`, `bios attest`, drills, and other write/power actions.
- `just docs-gen` regenerates `docs/doctor-checks.md` from the doctor registry.
  A drift test fails when the committed file does not match.
- `just hook-shellcheck` runs ShellCheck on the golden `nut-upssched-cmd` when the tool is installed.
- `just snap` reviews insta snapshots of rendered NUT files.

CI runs the same format/lint and test recipes on pushes to `main` and pull requests.
Actions are pinned by commit, permissions are read-only, and superseded runs are
canceled. Tests never require UPS hardware, NUT services, privileges, or network
access. Dependency acquisition and CI setup may require network access.
Host unit tests cover atomic replacement in a temporary directory under
`target/` and prove fake timeouts, failure injection, and UPS scripting
without contacting port 3493.

`just demo` starts `nut-fake-ups-d` on loopback with the checked-in sample fixture,
then runs the real CLI against it. `just fake-upsd` serves the fixture for manual
experiments. For a deliberate live read, `just remote-status HOST` and
`just remote-battery HOST` open an SSH tunnel to the target's loopback upsd; the
required host argument prevents accidental remote access. `just fixture HOST OUTPUT`
captures only the status fields shown by the CLI into a local JSON file. Set
`NUT_CONFIG` to a local host TOML file to include its shutdown timer.

SASE exposes `check` as `just check`, with declared gate inputs and toolchain
fingerprints. After every push, watch the CI run for the exact pushed SHA through
`/sase_monitor` before closing the phase.

`just install-smoke` installs into a temporary root, checks every command help,
and verifies both shell completion adapters. Bash and zsh are required; see
[completion](completion.md) for the protocol and provider extension points.
