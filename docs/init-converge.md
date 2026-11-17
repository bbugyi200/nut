# Init convergence

`init::converge` coordinates a reviewed init transaction end to end. The CLI
and rollback commands compose it later; these library APIs deploy nothing by
themselves.

`converge` requires root, refuses incomplete plans, and re-observes and
re-checks the authoritative plan hash under the shared writer lock before any
target mutation, entropy generation, or backup creation. An empty plan returns
before locking, generating entropy, or writing anything, and never prints a
hash mismatch: a second identical init reports zero changes. A pending journal
resumes instead: the retried run must carry the same reviewed hash, or
converge refuses and asks for a fresh reviewed plan.

Operations run in plan order grouped by kind: packages, the operator group,
memberships, fstab edits, and device repair first, then managed files, then
legacy-hook retirement, then services in driver/server/monitor order. Journal
IDs match the reviewed plan step IDs. Restarts always execute so units pick up
new configuration; already-satisfied enables complete without effects. Every
start or restart of the monitor, and every disruption of an already configured
stack, needs a fresh OL reading with none of OB, LB, or FSD immediately
before the effect. An install from empty may start its driver and server to
obtain its first reading. Ubuntu's NUT 2.8.1 runs upsd and upsmon as
`Type=simple` units, so a just-started server may not be listening yet, and
a server that just connected to its driver publishes only `ups.status: WAIT`
until the driver answers its first dump: an unavailable, stale, or `WAIT`
reading is re-observed every 0.5 s for about 15 s, while an explicit OB, LB,
or FSD reading refuses at once. Service outcomes are recorded only after verifying
the unit reached the desired state; a zero exit code alone never counts. No
code path runs power commands.

Adopted credentials come from the immutable backup preimages, falling back to
live files only when the target did not exist before the transaction. Rotation
and generation materialize once into the backup directory; a retry reuses the
same blobs instead of drawing new entropy.

Post-apply verification runs a fast structured doctor pass with normal
non-strict semantics and no observation sleeps. Warnings stay warnings; any
failure, or unavailable required protection evidence, fails verification.
While every failure is only a readiness gap of a just-started unit (driver,
server, or monitor inactive, nothing listening, no logged-in monitor client,
or no UPS data yet, including upsd's `WAIT` placeholder), the pass is repeated within the same bounded settle
window; any other failure, including an on-battery reading, fails at once. On
failure, converge restores touched files and service enable/active states from
the recorded preimages, keeps the backup and the recovery record, and reports
the exact `nut rollback <stamp>` command. The state record is finalized only
after verification passes. Its output hashes and metadata, explicit
absent-output set, and actual group-creation record feed the `state.drift` and
`state.backup` doctor checks and rollback.

See [init rollback](init-rollback.md) for the validated restore API, service
safety gates, and the limits of package rollback.
