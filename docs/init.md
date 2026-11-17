# Init and rollback

`nut init` converges host NUT configuration through a reviewed, recoverable
transaction. `nut rollback` restores the recorded pre-init files, metadata,
service state, and membership additions. A second successful init has zero
effects. Neither command is on the outage path: NUT daemons, `upssched`, and
the rendered POSIX hook still own shutdown.

See [init convergence](init-converge.md), [init resources](init-resources.md),
and [init rollback](init-rollback.md) for the library contracts. This page is
the operator guide.

## Preview, hash, and apply

`nut init --dry-run` (`-n`) prints the deterministic plan and never elevates,
creates a lock, generates secrets, or writes state. Add `--json` for a
schema-versioned document with `complete`, `hash` when authoritative, redacted
steps, and explanations. A complete empty plan prints `Already converged`.

`--expect-plan HASH` requires `--yes` and binds apply to that reviewed hash.
Root recomputes the plan; a mismatch performs no target writes. Direct root
`nut init --yes` without `--expect-plan` is available for isolated CI. Agents
on real hosts must obtain a root preview hash through the reviewed sudo
workflow and pass `--expect-plan`.

An ordinary interactive init shows the plan, then asks for confirmation.
Non-root elevation requires interactive stdin/stdout and the running
executable to be the protected, root-owned `/usr/local/bin/nut` with safe
ancestors. The elevated argv is exactly:

```
sudo -- /usr/local/bin/nut init --yes --expect-plan HASH --config ABS
```

`--rotate-secrets` is carried when selected. Non-root noninteractive `--yes`
never initiates sudo. Cancellation of either prompt has no configuration
effects.

## Privileged preview

If required inputs are unreadable, the plan is partial: it has no apply hash
and never prints `Already converged`. An interactive non-root run may offer a
read-only privileged preview using the installed root-owned binary, then show
that complete masked plan and ask for apply confirmation. Bind the final
invocation to that preview's hash. `--expect-plan` never accepts a partial
plan. Dry-run, JSON, and noninteractive previews never prompt or elevate.

Without a host config, an interactive run offers `nut config new` scaffolding.
Noninteractive and dry-run paths return that guidance without writing.

## Secrets, rotation, and ACL

Adopt existing valid upsmon credentials from `upsd.users` and the matching
MONITOR line. Adopt the operator credential from `/etc/nut-cli/operator.secret`
and/or the matching NUT account when unambiguous. Missing credentials generate
only the missing secret. Conflicting or malformed records fail with a masked
explanation unless `--rotate-secrets` replaces the pair.

`--rotate-secrets` rotates both managed credentials once per transaction.
Generated material is stored privately before either endpoint changes, so a
retry resumes that transaction instead of drawing a second pair. Secrets never
enter argv, environment, logs, diffs, or public JSON.

The operator file is exactly one password plus a newline, owned
`root:nut-operators` mode `0640`. `nut-operator` is a NUT account, not a Unix
login. Its only grants are `beeper.disable`, `beeper.enable`, `beeper.mute`,
`test.battery.start.quick`, and `test.battery.stop`. Authenticated action
protocol tests are owned by a later phase; this tree documents the file and
ACL so that work can extend the dummy-ups harness.

Optional `[operators] journal = true` adds `systemd-journal` membership and
exposes all system logs. Init never enables that membership unless the host
config says so. `nut events` may still need `sudo nut events` without it.

## Recovery and rollback limits

A nonempty apply snapshots every affected preexisting file, including absent
targets, before effects. Interrupted transactions resume from durable
checkpoints when current targets match the recorded before/after
possibilities. Unrelated drift requires a fresh reviewed plan.

`nut rollback [STAMP] --dry-run` and `--list` never mutate or elevate. Restore
needs root and confirmation (`--yes` for reviewed automation/CI). Rollback
restores original bytes and metadata, removes files that were absent before
init, reverses only recorded membership additions, and restores recorded
service enable/active state under the same safe-online gate (stops after the
monitor is down need no fresh reading).

Installed distribution packages and package-owned users are retained. Rollback
does not uninstall `nut-client`/`nut-server` or restore `/etc/group` from a
raw snapshot. The plan and JSON name that boundary.

Exact recovery commands:

- `nut init --dry-run --json` — review the current plan and hash
- `sudo nut init --yes --expect-plan HASH` — apply a reviewed plan
- `nut rollback --list` — redacted backup summaries
- `sudo nut rollback --yes STAMP` — restore that transaction
- `sudo nut doctor` — structured protection checks, including `state.drift`
  and `state.backup`
- `sudo nut doctor state.drift` — applied outputs versus the state record
- `sudo nut doctor state.backup` — backup manifest and blobs for that record

## Dummy-ups evidence

Isolated Ubuntu 24.04 CI installs NUT 2.8.1, the root-owned `/usr/local/bin/nut`,
and a local dummy-ups `.dev` source. It proves hash-bound init, the root
`init --yes` path, dummy USB/outlet-command skips, a no-op second init, dummy
OB/OL event recovery, and rollback of the selected stamp. `just check` stays
offline; `just e2e` is the disposable job. See [development](development.md).

## Related

- [Getting started](getting-started.md)
- [Host configuration](configuration.md)
- [Safety contract](safety.md)
- [CLI reference](reference/cli/init.md)

