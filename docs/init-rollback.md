# Init rollback

The rollback library restores a completed `nut init` transaction from its
private backup. Its API is `init::rollback::list_backups`, `plan`, and
`restore`, bound to `nut rollback [STAMP] [-l/--list] [-n/--dry-run] [-y/--yes]`
with redacted JSON and width-aware screens. List and dry-run never mutate or
elevate; restore needs root and an explicit confirmation, elevating through the
reviewed `sudo -- /usr/local/bin/nut` argv only from an interactive terminal
with a root-owned binary.

## Review and selection

`list_backups` returns redacted summaries. `plan(host, None)` previews the
newest backup; `plan(host, Some(stamp))` previews an explicit stamp. A preview
validates the complete manifest, every saved file blob, protected ownership
and modes, allowlisted absolute paths, and current target state. It never
returns saved file contents. If the newest backup is corrupt, selection fails
instead of silently falling back to an older transaction.

Rollback refuses to run while an init transaction is pending. It only restores
the backup referenced by the current state record, unless a rollback progress
record proves that the selected rollback already began. Drifted targets must be
reviewed before any restore can overwrite them.

## Restore behavior

`restore` acquires the same writer lock used by init and checkpoints progress
after each restored file, membership, group, and service. Its private
`rollback.json` record lives inside the selected backup directory. If the
process stops between an effect and its checkpoint, a retry recognizes the
target's preimage and continues. The original backup is retained.

Files are restored atomically with their original bytes, owner, group, and
mode. Targets that were absent before init are removed. The prior state record
is restored last. Rollback reverses only the exact `nut-operators` and
`systemd-journal` memberships recorded as additions by that init transaction;
other group members remain. An operator group is removed only when the backup
and finalized state both prove that init created it, and the group has no
supplementary members or primary-group users. Otherwise it is retained.

Installed distribution packages and package-owned users are retained. Rollback
does not restore `/etc/group`, package databases, or account databases from raw
file snapshots.

The prior service enablement and active state are restored after configuration
files. Starting any service, stopping the monitor, and stopping any other NUT
unit while the monitor still runs require a fresh UPS status containing `OL`
and none of `OB`, `LB`, or `FSD`. Units are stopped monitor first, then the
server, then the driver; once the monitor is stopped no shutdown authority
remains, so the server and driver stop without re-observing upsd, which the
same rollback may already have stopped. A missing or unsafe observation leaves the
backup and progress record in place; retry the same rollback after the UPS is
safely online. A service that was inactive before init stays stopped. If the
restored UPS configuration is absent, rollback refuses to start a driver or
monitor.

## Doctor state checks

`state.drift` validates the protected state record, installed host config,
recorded managed file hashes and metadata, explicit absent outputs, and the
presence of pending recovery. It distinguishes no prior init, unreadable state,
corrupt state, pending recovery, and applied-file drift.

`state.backup` verifies that the state record's backup reference points to a
complete, private manifest and matching preimage blobs. Both checks require
root because the state and backup trees are root-only. Run `nut doctor --list`
for their generated catalog descriptions and fix commands.
