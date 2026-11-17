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
   commands must not copy captured output into error text.
5. **Keep recipes offline.** Build and test recipes must never touch a real UPS,
   power anything off, or implicitly contact another host. Any future recipe that
   reaches a host must require that host as an explicit argument.
6. **Claims need evidence.** Protection claims require live readings or recorded
   attestations. Unknown state must be stated plainly, along with how to verify it.
