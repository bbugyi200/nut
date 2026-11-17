# Safety contract

These rules apply to every command and implementation phase.

1. **Management plane only.** NUT daemons, `upssched`, and a tiny rendered shell hook
   own outage handling. No `CMDSCRIPT`, `NOTIFYCMD`, or `SHUTDOWNCMD` may invoke this
   binary. A missing or broken CLI must never disable shutdown protection.
2. **Reads by default.** Writes are planned, diffed, backed up, and confirmed.
   `doctor` must never fix anything, and every warning must name an exact remedy.
3. **Agents cannot trigger power events.** `nut drill *`, `nut bios setup`, and
   `nut battery test` require interactive stdin and stdout. Drills and BIOS setup
   also require the hostname to be typed and must refuse `--yes`. No code path may
   send `load.off`, `shutdown.*`, `driver.killpower`, deep battery tests, or injected
   ONBATT/LOWBATT/FSD events. Operator permissions must allow only `beeper.*`,
   `test.battery.start.quick`, and `test.battery.stop`, never `ALL`. Offline tests
   use a fake Host for power actions. Agent init apply requires a reviewed sudo
   gate and `--expect-plan`.
   Starting a physical drill requires root, interactive stdin and stdout, the
   typed hostname, a live `OL` UPS at 95% charge or higher, and persistent
   journaling. The command writes a pending evidence record before showing the
   keyboard steps, refuses `--yes`, and never invokes shutdown, outlet, or NUT
   power commands itself. See
   [drill guidance](drills.md).
4. **Protect secrets.** Passwords must never enter argv, logs, diffs, verbose output,
   or JSON. Diffs mask passwords; operator credentials belong in a file restricted
   to the appropriate group. Privileged file writes use same-directory temporary
   files, ownership and mode before publication, fsync, and atomic rename. Secret-bearing
  commands must not copy captured output into error text. Init transaction preimages
  live only in root-owned `0700` backup directories and `0600` blobs. Pending
  journals contain references and fingerprints, while generated credentials are
  persisted privately once before either credential endpoint is changed. Recovery
  compares started operations to recorded before/after states; unknown results remain
  unresolved until freshly observed. Rollback validates the complete backup, uses the
  same writer lock, and records resumable progress. It restores only recorded targets
  and account additions, retains installed packages and package-owned users, and
  requires fresh safe-online UPS evidence before changing service activity. See
  [init rollback](init-rollback.md).
5. **Keep recipes offline.** Build and test recipes must never touch a real UPS,
   power anything off, or implicitly contact another host. Any recipe that
   reaches a host must require that host as an explicit argument. `just check`
   stays offline. Real NUT integration runs only in the disposable
   `just e2e` job, which installs runner-only shutdown and sendmail stubs
   before starting NUT and asserts the shutdown helper was never invoked.
6. **Claims need evidence.** Protection claims require live readings or recorded
   attestations. Unknown state must be stated plainly, along with how to verify it.
   Isolated dummy-ups CI records init-to-init empty plans, dummy OB/OL event
   recovery, rollback round trips, and the read-only athena `init --dry-run`
   preview. Dummy-ups skips USB matching and `shutdown.return`; it does not
   disable physical-device checks for real UPS profiles.

Init apply on a real host requires `/sase_sudo` and `--expect-plan`. Rollback
retains installed distribution packages; recover files with
`sudo nut rollback --yes STAMP` and inspect drift with
`sudo nut doctor state.drift`. See [init and rollback](init.md).
