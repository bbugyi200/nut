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
  `status`, `doctor`, `init --dry-run`, `rollback --list/--dry-run`, and the other
  allowlisted verbs). It refuses `config new/render`, `bios setup`, `bios attest`,
  drills, init apply, rollback restore, and other write/power actions.
- `just docs-gen` regenerates `docs/doctor-checks.md`, `docs/reference/cli/*.md`,
  `man/*.1`, and the demo screenshot. Set `NUT_UPDATE_GOLDENS=1` (the recipe does).
  Drift tests fail when the committed files do not match.
- `just screenshot` is an alias for `just docs-gen` and refreshes
  `docs/screenshot.txt` plus `docs/screenshot.svg` from the scripted demo UPS.
- `just install` builds a release binary, installs it root-owned at
  `/usr/local/bin/nut`, installs the man pages, and installs completion as the
  user. It calls `sudo`. Agents must use `/sase_sudo`.
- `just install-smoke` installs into a temporary root, walks every visible
  command's `--help`, runs a read-only invocation of every command group, and
  verifies both shell completion adapters.
- `just clean` removes Cargo build artifacts.
- `just hook-shellcheck` runs ShellCheck on the golden `nut-upssched-cmd` when the tool is installed.
- `just snap` reviews insta snapshots of rendered NUT files.

CI runs the same format/lint and test recipes on pushes to `main` and pull requests.
Actions are pinned by commit, permissions are read-only, and superseded runs are
canceled. Tests never require UPS hardware, NUT services, privileges, or network
access. Dependency acquisition and CI setup may require network access.

A separate `e2e.yml` job on `ubuntu-24.04` installs NUT 2.8.1, fails if that
version is unavailable, builds with `--locked` as the unprivileged runner,
passes that binary to the root harness through `NUT_E2E_BIN` (sudo's
`secure_path` hides runner tools, so the job calls `just` by absolute path),
installs it root-owned at `/usr/local/bin/nut`, and runs `just e2e` against a local
dummy-ups `.dev` source. The harness proves hash-bound init, root `init --yes`,
idempotent reruns, dummy OB/OL events, and rollback. Runner-only shutdown and
sendmail stubs are installed before NUT starts by renaming the originals aside,
never copying through a symlink: on usrmerge Ubuntu `/sbin/shutdown` and
`/usr/sbin/shutdown` are one symlink to `systemctl`, and the harness asserts
`systemctl` survives. Because init's post-apply verification includes
`boot.fstab`, the harness comments out runner fstab entries that are neither
mounted nor active swap when `findmnt --verify` fails, and stops with the
`findmnt` report if it still fails. `just e2e-lint` validates those
fixtures during `just check` without contacting a host or starting NUT.
Host unit tests cover atomic replacement in a temporary directory under
`target/` and prove fake timeouts, failure injection, and UPS scripting
without contacting port 3493.

The init transaction store lives under `src/init/transaction.rs`. Its
transaction handle retains the shared `/var/lib/nut-cli.init.lock` for the full
writer operation. Callers that need to hash-check a re-observation under the
lock use `acquire_writer_lock` and transfer the guard to `begin_locked`. A
nonempty transaction snapshots every allowlisted target, including absent
targets and the prior state record, into root-owned `0700` backup directories
with `0600` manifest and blob files. The protected pending journal is
published only after the complete manifest. File operations carry before/after
fingerprints and durable planned/started/completed checkpoints; recovery
compares started operations to current state and leaves unobserved external
operations unresolved. A caller must not infer that a started operation did
not run merely because its completion checkpoint is absent.

Newly generated credentials are hex materialized into private blobs after the
pending journal exists and before their first consumer file is changed. A
retry reuses the existing blob. The state record is atomically published only
after verification. Its intent and publication are journaled before the pending
record is removed. If interrupted after writing state but before recording
publication, recovery compares the state bytes with the intent and resumes
without rewriting them. Empty plans return before taking the lock,
creating directories, generating entropy, or writing backups or state. The
`init::converge` composes the transaction store with resource application,
service effects, and verification. `init::rollback` exposes validated list,
preview, and restore APIs. Restore shares the writer lock, validates the
manifest and blobs before effects, and writes a private per-backup progress
journal after each effect so retries are idempotent. `state.drift` and
`state.backup` are root-only structured doctor checks over the protected state
record, its output fingerprints, and its backup reference. See
[init resource application](init-resources.md),
[init convergence](init-converge.md), and [init rollback](init-rollback.md)
for the executor, coordinator, and recovery contracts. `init::cli` binds them
to `nut init` and `nut rollback` with plan-hash confirmation, root-owned
binary checks, privileged preview elevation, redacted JSON, and width-aware
screens.

`just demo` starts `nut-fake-ups-d` on loopback with the checked-in sample fixture,
then runs the real CLI against it. `just fake-upsd` serves the fixture for manual
experiments. For a deliberate live read, `just remote-status HOST` and
`just remote-battery HOST` open an SSH tunnel to the target's loopback upsd; the
required host argument prevents accidental remote access. `just fixture HOST OUTPUT`
captures only the status fields shown by the CLI into a local JSON file. Set
`NUT_CONFIG` to a local host TOML file to include its shutdown timer.

SASE exposes `check` as `just check`, with declared gate inputs and toolchain
fingerprints. After every push, watch the CI and e2e runs for the exact pushed
SHA through `/sase_monitor` before closing the phase.

`release.yml` runs on `v*` tags. It builds a static `x86_64-unknown-linux-musl`
binary, writes a SHA-256 checksum, and attaches both files to the GitHub
Release. Cut `v0.1.0` only after athena adoption.

Read-only athena preview uses an explicit host and an explicit config path:

```
just remote athena --config PATH init --dry-run
just remote athena --json --config PATH init --dry-run
```

Do not install production `/etc/nut-cli/nut.toml` just to obtain that evidence.
A temporary redacted config may be staged for the preview and removed afterward.
A non-root preview is partial when `/etc/nut` is unreadable: it names those
files, intended operator and hook changes, and the required root preview. Root
dry-run and apply on athena remain the adopt phase's responsibility. Recorded
preview output lives in [athena preview](athena-preview.md).

`just install-smoke` is required in CI. Bash and zsh are required; see
[completion](completion.md) for the protocol and provider extension points.

## Related

- [Getting started](getting-started.md)
- [Architecture](architecture.md)
- [CLI reference](reference/cli/index.md)

