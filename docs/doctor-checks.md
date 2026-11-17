# Doctor checks

Generated from the nut doctor registry. Do not edit by hand; run `just docs-gen`.

## This tool

### `tool.config`

The selected host config exists and parses as schema 1. Discovery still runs when the file is missing.

Needs root: no

### `tool.binary`

The running `nut` binary should be the root-owned `/usr/local/bin/nut` that matches this version.

Needs root: no

### `tool.completion`

Installed bash/zsh adapters match this build. Missing adapters are a warning, not a failure.

Needs root: no

## Hardware

### `hw.usb`

Sysfs lists exactly one USB device matching the UPS profile, and its `/dev/bus/usb` node is group `nut`. Skipped for dummy-ups.

Needs root: no

### `hw.data`

upsd answers LIST VAR without DATA-STALE or DRIVER-NOT-CONNECTED.

Needs root: no

### `hw.online`

Status includes OL and none of OB, LB, or FSD.

Needs root: no

### `hw.delays`

Live `ups.delay.shutdown` and `ups.delay.start` match the resolved profile (60/0 on athena).

Needs root: no

### `hw.shutdown-return`

LIST CMD includes `shutdown.return` so the UPS can restore outlets after a clean halt.

Needs root: no

### `hw.rb`

The RB status flag is a warning. Doctor never starts a battery test.

Needs root: no

### `hw.charge`

Charge below 90% is a warning. The 95% runtime rule lives in `protect.runtime`.

Needs root: no

### `hw.battery-age`

When `battery.installed` is set, warn once the pack is older than three years.

Needs root: no

### `hw.self-test`

Reports `ups.test.result`. Doctor never initiates a test.

Needs root: no

## Services

### `svc.driver`

`nut-driver@<ups>.service` is active.

Needs root: no

### `svc.server`

`nut-server.service` is active.

Needs root: no

### `svc.monitor`

`nut-monitor.service` is active.

Needs root: no

### `svc.enabled`

The driver, server, monitor, and `nut.target` are enabled.

Needs root: no

### `svc.restarts`

With `--observe SECS`, the driver stays active and NRestarts does not increase. Skipped unless observe is set.

Needs root: no

### `svc.listen`

Port 3493 listeners in `/proc/net/tcp` and `/proc/net/tcp6` are loopback addresses.

Needs root: no

### `svc.client`

LIST CLIENT includes a loopback address for this UPS.

Needs root: no

### `svc.version`

Require NUT 2.8.1 or newer. 2.8.3 and later warn so CyberPower default-delay changes can be reviewed.

Needs root: no

## Protection

### `protect.runtime`

When charge is at least 95%, `early_after + shutdown_budget + offdelay + 60s` must fit the live runtime (athena's 420 s rule).

Needs root: no

### `protect.poweroff-wait`

`POWEROFF_WAIT` must be at least full-charge runtime plus 10 minutes so the reboot net cannot fire on battery.

Needs root: no

### `protect.hook`

`/usr/local/sbin/nut-upssched-cmd` (or the legacy athena hook) exists and is executable.

Needs root: no

### `protect.mail`

`/usr/sbin/sendmail` is present so the hook can submit notification mail.

Needs root: no

### `protect.pwrstatd`

`pwrstatd` must not be running. Doctor never stops it.

Needs root: no

### `protect.killpower`

A leftover `/etc/killpower` is reported and never cleared.

Needs root: no

## Root-only

### `root.drift`

Compare rendered files with live paths, secrets masked. Skipped without root.

Needs root: yes

### `root.credentials`

The upsmon password in `upsd.users` matches the MONITOR line. Secrets are never printed.

Needs root: yes

### `root.placeholders`

Live files must not contain `@UPSMON_PASSWORD@` or `@OPERATOR_PASSWORD@`.

Needs root: yes

### `root.instcmds`

Operator and upsmon accounts must not grant `instcmds = ALL`.

Needs root: yes

### `root.perms`

`/etc/nut/*` is `root:nut` 0640; the hook is `root:root` 0755.

Needs root: yes

## Boot & BIOS

### `boot.efi-order`

Reuses the BIOS boot-readiness audit.

Needs root: no

### `boot.nofail`

Host-config `boot.nofail_mounts` carry `nofail` in fstab.

Needs root: no

### `boot.fstab`

Reuses the BIOS boot-readiness audit.

Needs root: no

### `boot.journal`

Reuses the BIOS boot-readiness audit.

Needs root: no

### `boot.panic`

Advisory: a hang will not reboot when kernel.panic=0.

Needs root: no

### `boot.watchdog`

Advisory hardware watchdog check from the BIOS audit.

Needs root: no

### `bios.profile`

DMI board_vendor/board_name match a board profile, or the generic fallback.

Needs root: no

### `bios.attest`

A changed BIOS version or efivar fingerprint is a warning, never a failure.

Needs root: no

## Proof

### `proof.blip`

Shows the latest 60-second mains blip result and marks it stale when BIOS settings, battery date, NUT version, or NUT/host configuration changes.

Needs root: no

### `proof.ac-loss`

Shows the latest true AC-loss recovery result and its BIOS, battery, NUT, and configuration freshness.

Needs root: no

### `proof.cycle`

Shows the latest UPS outlet-cycle recovery result and its BIOS, battery, NUT, and configuration freshness.

Needs root: no

### `proof.outage`

Shows the latest sustained outage recovery result and its BIOS, battery, NUT, and configuration freshness.

Needs root: no
