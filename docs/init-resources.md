# Init resource application

`init::resources` executes the resource portion of a reviewed init transaction.
The CLI and service coordinator compose it with the planner, durable secret
store, fresh online checks, doctor verification, and final state publication.
These library APIs do not deploy anything by themselves.

Construct typed `Resource` values from the approved plan. The executor supports
missing packages, the `nut-operators` system group, additive Unix memberships,
one selected USB device, managed files, requested `fstab` nofail edits, and
retirement of the recognized legacy hook. It does not start services, remount
filesystems, or run power actions. Configured users must already exist; journal
membership is constructed only when explicitly enabled in the host config.

The coordinator takes the shared writer lock, re-observes and validates the
reviewed plan hash before beginning the transaction. Use `Resource::prepare`
for operations whose exact output is already known; it returns `None` when
contents and metadata already match. Include each prepared intent, each
`backup_targets()` path, the prior state path, service snapshots and the exact
membership additions in `BeginRequest`. Retain the prepared resource until its
operation settles. An apply with a mismatching intent or missing backup refuses
before effects.

For generated credentials or package-created numeric group IDs, reserve the
operation ID with `reserve_resource` before beginning the transaction and back
up every eventual target first. Materialize credentials through the transaction
store, render/escape the adopted or durable credentials, prepare the resource,
then call `bind_resource` once. Binding publishes its exact fingerprints before
its first effect. It does not change the original backup. Deferred files use
external observations for recovery, including the destination path in their
fingerprints. In particular, capture a package-created default file as the
resource's intermediate preimage only after the package operation is verified;
the backup must still contain its original absent marker.

`apply_resource` checks actual state, journals intent, performs the operation,
verifies the result and journals completion. A command exiting zero without the
requested state is a failure. A started operation already at its desired state
only finishes its checkpoint and temporary-file cleanup. A completed operation
has no further effects. Unexpected drift is refused.

After restart, use `recover_with_observations` for external operations and
`resume_resource` with the reviewed payload to reconstruct an executor from the
original journal. Do not re-prepare a started operation against its current
state and substitute a new preimage. Payload bytes have no Debug or Serialize
implementation. Captured package/identity-tool output is never printed in
resource errors.

## Packages and accounts

`Resource::packages` simulates the requested missing NUT packages and returns the
full missing dependency closure with exact versions in `package_versions()`.
The coordinator must include that sorted closure in its authoritative review
hash. The executor simulates that same pinned set immediately before installing
and rejects added dependencies, removals, upgrades, installed dependencies and
unplanned configuration operations. Apt receives `--no-upgrade`, `--no-remove`,
`--no-install-recommends`, version pins and `--force-confold`. A partially
configured dpkg database is refused with recovery guidance rather than treated
as an absent package. Distribution packages remain installed on rollback.

Before apt, the transaction backs up `/usr/sbin/policy-rc.d` and every managed
NUT config that apt may create. If no policy existed, an atomic temporary policy
returns 101 for NUT units and leaves other units permitted. The policy is removed
on success and ordinary failure. A process interruption retains its journal;
`restore_package_policy` can remove the exact owned policy during recovery,
without rerunning apt. An existing policy remains byte-for-byte unchanged and
must deny NUT starts and restarts; a permissive policy causes a refusal before
installation. Changed or unsafe policy files are retained for reviewed recovery.
These semantics follow Debian's [policy interface](https://manpages.debian.org/unstable/init-system-helpers/invoke-rc.d.8.en.html)
and [apt simulation interface](https://manpages.debian.org/testing/apt/apt-get.8.en.html).

Membership effects use `usermod --append --groups`, never replace the existing
membership list, and verify numeric groups through NSS. No Unix `nut-operator`
login is created. Create/verify `nut-operators` before preparing its dependent
memberships and files. The protected operator secret format is exactly one
password followed by a newline, owned by root:nut-operators with mode 0640.

## Devices, files and retirement

USB effects bind a validated sysfs selector, vendor/product, optional serial and
expected `nut` group ID. Bus/device numbers identify the actual `/dev/bus/usb`
node. Udev receives only that selector, then settles and the executor verifies
the node group again. An already correct node has no repair operation. Dummy
profiles produce no device operation.

All file publication uses protected no-follow atomic replacement with final
metadata set before rename. The five NUT config files are root:nut 0640; the
hook is root:root 0755; the installed host config is root:root 0644. A missing
managed parent may be created only beneath a validated existing parent. Protected
reads below an absent parent report absence without creating it. Semantic config
comparison avoids copying an installed config onto itself through an alias or
rewriting it merely because its formatting differs.

Nofail editing preserves other fields, spacing, comments and original metadata.
Every requested mount must match exactly one entry. The exact candidate is
written privately in `/etc`, checked by `findmnt --verify --tab-file`, and only
then atomically published. Both nonzero status and reported parser/errors fail
the operation. Warnings remain warnings. Recovery removes an interrupted owned
validation candidate without rewriting an already published fstab.

Legacy retirement requires the known shell body to match the rendered
replacement after normalizing the historical tag/test-path names and comments.
Two recognizable strings alone are insufficient. The exact replacement and its
upssched reference must already be installed, and upsmon must have no retained
legacy reference. The old hook is backed up before removal.

Offline tests use `FakeHost` operation traces, injected failures and reconstructed
process state. They exercise package guards, no-upgrade binding, additive groups,
file modes and drift, checkpoint interruption, nofail validation, selected-device
repair, dummy skips and retirement. Run the complete offline gate with `just check`.
