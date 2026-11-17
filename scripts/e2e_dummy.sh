#!/usr/bin/env bash
# Isolated Ubuntu NUT 2.8.1 dummy-ups integration. Production apply; no
# post-apply edits that would fake idempotence. Runner-only stubs live in this
# disposable environment. Keep just check offline: this script is CI-only.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
FIXTURE_DIR=$ROOT/tests/fixtures/e2e
STUB_DIR=$ROOT/scripts/e2e_stubs
CONFIG=$FIXTURE_DIR/host.toml
DEV_OL=$FIXTURE_DIR/dummy.dev
DEV_OB=$FIXTURE_DIR/dummy-ob.dev
DEV_LIVE=/var/lib/nut/e2e.dev
NUT_BIN=/usr/local/bin/nut
EXPECTED_NUT=2.8.1
DIAG_DIR=${NUT_E2E_DIAG:-/tmp/nut-e2e-diag}
SHUTDOWN_LOG=${NUT_E2E_SHUTDOWN_LOG:-/var/tmp/nut-e2e-shutdown.log}
MAIL_LOG=${NUT_E2E_MAIL_LOG:-/var/tmp/nut-e2e-sendmail.log}
SNAP_DIR=
POLL_SECONDS=${NUT_E2E_POLL_SECONDS:-90}
TIMEOUT_SECS=${NUT_E2E_TIMEOUT:-900}

usage() {
    cat <<'EOF'
Usage: scripts/e2e_dummy.sh [--lint]

  --lint   Validate fixtures and script syntax without contacting NUT.
EOF
}

json_get() {
    python3 -c 'import json,sys
doc=json.load(sys.stdin)
path=sys.argv[1].split(".")
cur=doc
for key in path:
    if isinstance(cur, list):
        cur=cur[int(key)]
    else:
        cur=cur[key]
if cur is None:
    sys.exit(1)
if isinstance(cur, (dict, list)):
    json.dump(cur, sys.stdout)
else:
    print(cur)
' "$1"
}

json_has_flag() {
    python3 -c 'import json,sys
doc=json.load(sys.stdin)
flags=doc.get("status",{}).get("flags") or []
want=sys.argv[1]
sys.exit(0 if want in flags else 1)
' "$1"
}

json_stories_reflect() {
    python3 -c 'import json,sys
doc=json.load(sys.stdin)
needles=[n.lower() for n in sys.argv[1].split("|")]
blob=json.dumps(doc.get("stories") or []).lower()
sys.exit(0 if any(n in blob for n in needles) else 1)
' "$1"
}

redact() {
    python3 -c 'import re,sys
text=sys.stdin.read()
text=re.sub(r"(?im)^(.*password\s*=\s*).*$", r"\1 ***", text)
# upsmon.conf directive only: case-sensitive and line-local, so prose such as
# "monitor is unavailable" in doctor JSON is not mangled.
text=re.sub(r"(?m)^([ \t]*MONITOR[ \t]+\S+[ \t]+\S+[ \t]+\S+[ \t]+)\S+", r"\1***", text)
sys.stdout.write(text)
'
}

log() {
    printf 'e2e: %s\n' "$*"
}

fail() {
    printf 'e2e: FAIL: %s\n' "$*" >&2
    exit 1
}

preserve_diag() {
    mkdir -p "$DIAG_DIR"
    {
        echo "=== nut doctor --json ==="
        "$NUT_BIN" --json --config "$CONFIG" doctor 2>&1 | redact || true
        echo "=== systemctl NUT ==="
        systemctl status 'nut-driver@dummy.service' nut-server.service nut-monitor.service nut.target --no-pager 2>&1 | redact || true
        echo "=== journalctl nut* ==="
        journalctl -u 'nut-driver@dummy.service' -u nut-server.service -u nut-monitor.service --no-pager -n 200 2>&1 | redact || true
        echo "=== dummy.dev ==="
        cat "$DEV_LIVE" 2>/dev/null || true
        echo "=== findmnt --verify --verbose ==="
        findmnt --verify --verbose 2>&1 || true
        echo "=== shutdown log ==="
        cat "$SHUTDOWN_LOG" 2>/dev/null || true
    } >"$DIAG_DIR/failure.txt" || true
    log "preserved redacted diagnostics in $DIAG_DIR/failure.txt"
}

restore_policy() {
    if [[ -n ${POLICY_BACKUP:-} && -e $POLICY_BACKUP ]]; then
        mv -f "$POLICY_BACKUP" /usr/sbin/policy-rc.d 2>/dev/null || true
        POLICY_BACKUP=
    elif [[ -n ${POLICY_INSTALLED:-} ]]; then
        rm -f /usr/sbin/policy-rc.d
        POLICY_INSTALLED=
    fi
}

on_exit() {
    local status=$?
    if [[ $status -ne 0 ]]; then
        preserve_diag || true
    fi
    restore_policy
}

lint_only() {
    [[ -f $CONFIG && -f $DEV_OL && -f $DEV_OB ]] || fail "missing e2e fixtures"
    [[ -x $STUB_DIR/shutdown && -x $STUB_DIR/sendmail ]] || fail "stubs must be executable"
    bash -n "$0"
    python3 - <<PY
from pathlib import Path
root = Path("$ROOT")
config = (root / "tests/fixtures/e2e/host.toml").read_text()
assert 'profile = "dummy"' in config
assert 'port = "/var/lib/nut/e2e.dev"' in config
assert 'early_after = "30m"' in config
dev = (root / "tests/fixtures/e2e/dummy.dev").read_text()
for key in ("ups.status: OL CHRG", "battery.charge: 100", "battery.runtime: 7200", "ups.delay.shutdown: 60", "ups.delay.start: 0"):
    assert key in dev, key
ob = (root / "tests/fixtures/e2e/dummy-ob.dev").read_text()
assert "ups.status: OB DISCHRG" in ob
print("e2e fixtures ok")
PY
    lint_stub_helper
    log "lint ok"
}

# Offline regression: stubbing a usrmerge shutdown symlink must not write
# through it into systemctl, and a second alias or rerun keeps the backup.
lint_stub_helper() {
    local tmp
    tmp=$(mktemp -d)
    mkdir -p "$tmp/usr/bin" "$tmp/usr/sbin"
    ln -s usr/sbin "$tmp/sbin"
    printf 'real systemctl\n' >"$tmp/usr/bin/systemctl"
    ln -s ../bin/systemctl "$tmp/usr/sbin/shutdown"
    stub_helper "$STUB_DIR/shutdown" "$tmp/sbin/shutdown"
    stub_helper "$STUB_DIR/shutdown" "$tmp/usr/sbin/shutdown"
    [[ $(cat "$tmp/usr/bin/systemctl") == "real systemctl" ]] \
        || { rm -rf "$tmp"; fail "stub_helper wrote through the shutdown symlink"; }
    [[ $(readlink "$tmp/usr/sbin/shutdown.nut-e2e-orig") == ../bin/systemctl ]] \
        || { rm -rf "$tmp"; fail "stub_helper lost the original shutdown symlink"; }
    cmp -s "$STUB_DIR/shutdown" "$tmp/usr/sbin/shutdown" \
        || { rm -rf "$tmp"; fail "stub_helper did not install the shutdown stub"; }
    rm -rf "$tmp"
    log "stub install keeps usrmerge systemctl intact"
}

snapshot_state() {
    local dest=$1
    mkdir -p "$dest/files"
    python3 - "$dest" <<'PY'
import json, os, subprocess, sys
from pathlib import Path
dest = Path(sys.argv[1])
paths = [
    "/etc/nut/nut.conf",
    "/etc/nut/ups.conf",
    "/etc/nut/upsd.users",
    "/etc/nut/upsmon.conf",
    "/etc/nut/upssched.conf",
    "/usr/local/sbin/nut-upssched-cmd",
    "/etc/nut-cli/nut.toml",
    "/etc/nut-cli/operator.secret",
    "/var/lib/nut-cli/state.json",
]
records = []
for path in paths:
    p = Path(path)
    rec = {"path": path, "exists": p.exists()}
    if p.exists():
        st = p.stat()
        rec.update({
            "uid": st.st_uid,
            "gid": st.st_gid,
            "mode": st.st_mode & 0o7777,
            "mtime_ns": st.st_mtime_ns,
            "sha256": subprocess.check_output(["sha256sum", path], text=True).split()[0],
        })
    records.append(rec)
(dest / "files.json").write_text(json.dumps(records, indent=2) + "\n")
PY
    getent group nut-operators >"$dest/nut-operators.group" || true
    getent group nut >"$dest/nut.group" || true
    {
        for unit in 'nut-driver@dummy.service' nut-server.service nut-monitor.service nut.target; do
            printf '%s active=%s enabled=%s enter=%s\n' \
                "$unit" \
                "$(systemctl is-active "$unit" || true)" \
                "$(systemctl is-enabled "$unit" || true)" \
                "$(systemctl show "$unit" -p ActiveEnterTimestampMonotonic --value || true)"
        done
    } >"$dest/services.txt" 2>&1 || true
    dpkg-query -W -f='${Package} ${Version}\n' nut-client nut-server >"$dest/packages.txt"
}

file_hash() {
    local path=$1
    if [[ -e $path ]]; then
        sha256sum "$path" | awk '{print $1}'
    else
        printf 'ABSENT\n'
    fi
}

file_mtime() {
    local path=$1
    if [[ -e $path ]]; then
        stat -c '%Y' "$path"
    else
        printf 'ABSENT\n'
    fi
}

assert_acl() {
    python3 - <<'PY'
import grp, os, pwd, sys
from pathlib import Path

def gid(name):
    try:
        return grp.getgrnam(name).gr_gid
    except KeyError:
        sys.exit(f"missing group {name}")

checks = [
    ("/etc/nut/nut.conf", 0, gid("nut"), 0o640),
    ("/etc/nut/ups.conf", 0, gid("nut"), 0o640),
    ("/etc/nut/upsd.users", 0, gid("nut"), 0o640),
    ("/etc/nut/upsmon.conf", 0, gid("nut"), 0o640),
    ("/etc/nut/upssched.conf", 0, gid("nut"), 0o640),
    ("/usr/local/sbin/nut-upssched-cmd", 0, 0, 0o755),
    ("/etc/nut-cli/nut.toml", 0, 0, 0o644),
    ("/etc/nut-cli/operator.secret", 0, gid("nut-operators"), 0o640),
]
for path, uid, gid_, mode in checks:
    st = Path(path).stat()
    got = (st.st_uid, st.st_gid, st.st_mode & 0o7777)
    if got != (uid, gid_, mode):
        sys.exit(f"{path} uid/gid/mode {got} != {(uid, gid_, mode)}")
print("acl ok")
PY
}

# Replace one runner helper with a stub without ever writing through a
# symlink. On usrmerge Ubuntu /sbin aliases /usr/sbin and shutdown is a
# symlink to systemctl, so copy-based backups or installs onto the link would
# overwrite systemctl itself. Rename the original entry aside instead.
stub_helper() {
    local stub=$1 target=$2 dir path
    dir=$(realpath -e "$(dirname "$target")")
    path=$dir/$(basename "$target")
    if [[ -e $path || -L $path ]] && [[ ! -e $path.nut-e2e-orig && ! -L $path.nut-e2e-orig ]]; then
        mv -T "$path" "$path.nut-e2e-orig"
    fi
    rm -f "$path"
    install -m 0755 "$stub" "$path"
}

install_stubs() {
    export NUT_E2E_SHUTDOWN_LOG=$SHUTDOWN_LOG
    export NUT_E2E_MAIL_LOG=$MAIL_LOG
    : >"$SHUTDOWN_LOG"
    : >"$MAIL_LOG"
    local target systemctl_bin
    for target in /sbin/shutdown /usr/sbin/shutdown; do
        stub_helper "$STUB_DIR/shutdown" "$target"
    done
    stub_helper "$STUB_DIR/sendmail" /usr/sbin/sendmail
    systemctl_bin=$(readlink -f "$(command -v systemctl)")
    if cmp -s "$STUB_DIR/shutdown" "$systemctl_bin" || cmp -s "$STUB_DIR/sendmail" "$systemctl_bin"; then
        fail "stub installation replaced $systemctl_bin"
    fi
    systemctl --version >/dev/null || fail "systemctl is unusable after stub installation"
    [[ ! -s $SHUTDOWN_LOG ]] || fail "stub installation invoked the shutdown stub"
    log "shutdown and sendmail stubs installed; systemctl intact"
}

# Doctor's boot.fstab check is part of init's post-apply verification. Hosted
# runner images can ship cloud fstab entries for disks this VM does not have,
# so make the disposable runner's fstab verify before NUT is touched: comment
# out only entries that are neither mounted nor active swap. A remaining
# failure stops here with the findmnt report instead of a confusing
# post-apply compensation.
prepare_runner_fstab() {
    local report=$DIAG_DIR/findmnt-verify.txt
    if findmnt --verify --verbose >"$report" 2>&1; then
        log "runner fstab verifies"
        return 0
    fi
    log "runner fstab fails findmnt --verify; disabling unmounted entries in this disposable runner"
    cat "$report"
    cp -a /etc/fstab "$DIAG_DIR/fstab.orig"
    python3 - <<'PY'
import os
import subprocess
from pathlib import Path

fstab = Path("/etc/fstab")
swaps = {line.split()[0] for line in Path("/proc/swaps").read_text().splitlines()[1:] if line.split()}
out = []
for line in fstab.read_text().splitlines():
    fields = line.split()
    if not fields or fields[0].startswith("#") or len(fields) < 3:
        out.append(line)
        continue
    source, target, fstype = fields[0], fields[1], fields[2]
    if fstype == "swap":
        resolved = subprocess.run(["findfs", source], capture_output=True, text=True).stdout.strip() if "=" in source else source
        keep = resolved in swaps or os.path.realpath(resolved) in swaps
    else:
        keep = subprocess.run(["findmnt", "-rn", "--mountpoint", target], capture_output=True).returncode == 0
    if keep:
        out.append(line)
    else:
        print(f"e2e: disabled unmounted fstab entry: {line}")
        out.append(f"# nut-e2e disabled unmounted entry: {line}")
fstab.write_text("\n".join(out) + "\n")
PY
    if ! findmnt --verify --verbose >"$report" 2>&1; then
        cat "$report"
        fail "runner fstab still fails findmnt --verify; init verification would fail boot.fstab"
    fi
    log "runner fstab verifies after disabling unmounted entries"
}

install_nut_packages() {
    export DEBIAN_FRONTEND=noninteractive
    if [[ ! -e /usr/sbin/policy-rc.d ]]; then
        printf '#!/bin/sh\nexit 101\n' >/usr/sbin/policy-rc.d
        chmod 0755 /usr/sbin/policy-rc.d
        POLICY_INSTALLED=1
    else
        POLICY_BACKUP=$(mktemp)
        cp -a /usr/sbin/policy-rc.d "$POLICY_BACKUP"
        printf '#!/bin/sh\nexit 101\n' >/usr/sbin/policy-rc.d
        chmod 0755 /usr/sbin/policy-rc.d
    fi
    apt-get update
    apt-get install -y --no-install-recommends nut-client nut-server python3
    local version
    version=$(dpkg-query -W -f='${Version}' nut-server)
    log "installed nut-server $version"
    [[ $version == ${EXPECTED_NUT}* ]] || fail "expected NUT $EXPECTED_NUT, got $version"
    version=$(dpkg-query -W -f='${Version}' nut-client)
    [[ $version == ${EXPECTED_NUT}* ]] || fail "expected nut-client $EXPECTED_NUT, got $version"
    systemctl stop nut-monitor.service nut-server.service 'nut-driver@*.service' nut-driver-enumerator.service 2>/dev/null || true
    restore_policy
}

install_dummy_source() {
    install -d -o nut -g nut -m 0750 /var/lib/nut
    install -o nut -g nut -m 0664 "$DEV_OL" "$DEV_LIVE"
    # Confine listen addresses in this disposable runner. Init does not manage
    # upsd.conf. Only list ::1 when the runner's loopback has it, so upsd never
    # depends on an address this VM cannot bind.
    {
        echo "LISTEN 127.0.0.1 3493"
        if ip -6 addr show dev lo 2>/dev/null | grep -q 'inet6 ::1/'; then
            echo "LISTEN ::1 3493"
        fi
    } >/etc/nut/upsd.conf
    chown root:nut /etc/nut/upsd.conf
    chmod 0640 /etc/nut/upsd.conf
    log "upsd listeners: $(awk '{print $2}' /etc/nut/upsd.conf | paste -sd' ')"
}

install_cli() {
    local bin
    if [[ -n ${NUT_E2E_BIN:-} ]]; then
        # CI builds with --locked as the unprivileged runner and passes the binary.
        bin=$NUT_E2E_BIN
    else
        cargo build --locked --bin nut --release
        if [[ -n ${CARGO_TARGET_DIR:-} && -x ${CARGO_TARGET_DIR}/release/nut ]]; then
            bin=${CARGO_TARGET_DIR}/release/nut
        else
            bin=$ROOT/target/release/nut
        fi
    fi
    [[ -x $bin ]] || fail "release binary missing at $bin"
    install -o root -g root -m 0755 "$bin" "$NUT_BIN"
    local meta
    meta=$(stat -c '%u %a' "$NUT_BIN")
    [[ $meta == "0 755" ]] || fail "$NUT_BIN is not root-owned 0755 ($meta)"
}

write_dummy_dev() {
    local src=$1
    install -o nut -g nut -m 0664 "$src" "$DEV_LIVE"
    touch "$DEV_LIVE"
}

poll_until() {
    local label=$1
    shift
    local i
    for i in $(seq 1 "$POLL_SECONDS"); do
        if "$@"; then
            log "$label after ${i}s"
            return 0
        fi
        sleep 1
    done
    fail "timed out waiting for $label"
}

status_has() {
    local flag=$1
    "$NUT_BIN" --json --config "$CONFIG" status | json_has_flag "$flag"
}

events_reflect() {
    local needle=$1
    "$NUT_BIN" --json --config "$CONFIG" events | json_stories_reflect "$needle"
}

doctor_ok() {
    local json code
    set +e
    json=$("$NUT_BIN" --json --config "$CONFIG" doctor)
    code=$?
    set -e
    printf '%s\n' "$json" >"$DIAG_DIR/doctor.json"
    printf '%s\n' "$code" >"$DIAG_DIR/doctor.exit"
    python3 - "$DIAG_DIR/doctor.json" "$code" <<'PY'
import json,sys
doc=json.load(open(sys.argv[1]))
code=int(sys.argv[2])
checks={c["id"]:c for c in doc["checks"]}
usb=checks["hw.usb"]
if usb["status"] != "skipped" or "dummy-ups" not in usb["evidence"]:
    sys.exit(f"hw.usb was not skipped for dummy-ups: {usb}")
sr=checks["hw.shutdown-return"]
if sr["status"] != "skipped":
    sys.exit(f"hw.shutdown-return was not skipped: {sr}")
for req in ("hw.data", "hw.online"):
    if checks[req]["status"] != "pass":
        sys.exit(f"{req} status {checks[req]['status']}")
def environment(check_id):
    return check_id.startswith(("boot.", "bios.", "proof."))
protection_fails=[c["id"] for c in doc["checks"] if c["status"]=="fail" and not environment(c["id"])]
if protection_fails:
    sys.exit("protection checks failed: " + ",".join(protection_fails))
env_notes=[f'{c["id"]}={c["status"]}' for c in doc["checks"] if environment(c["id"]) and c["status"] in ("fail", "unknown")]
unknowns=[c["id"] for c in doc["checks"] if c["status"]=="unknown"]
if code != 0 and (unknowns or env_notes):
    # GitHub runners lack athena BIOS/boot/proof evidence. Required
    # protection checks already passed; record the remainder.
    print("doctor required checks passed; environment:", ",".join(env_notes or unknowns))
elif code != 0:
    sys.exit(f"doctor exited {code} without unknown or environment checks")
else:
    print("doctor exit 0")
PY
}

second_init_noop() {
    local before_state after_state before_backup after_backup json hash
    before_state=$(file_mtime /var/lib/nut-cli/state.json)
    before_backup=$(find /var/backups/nut-cli -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l)
    snapshot_state "$DIAG_DIR/pre-second"
    json=$("$NUT_BIN" --json --config "$CONFIG" init --dry-run)
    printf '%s\n' "$json" >"$DIAG_DIR/second-dry-run.json"
    python3 - "$DIAG_DIR/second-dry-run.json" <<'PY'
import json,sys
doc=json.load(open(sys.argv[1]))
assert doc["complete"] is True
assert doc["empty"] is True, doc
print("second dry-run empty")
PY
    hash=$(printf '%s' "$json" | json_get hash)
    "$NUT_BIN" --json --config "$CONFIG" init --yes --expect-plan "$hash" >"$DIAG_DIR/second-init.json"
    after_state=$(file_mtime /var/lib/nut-cli/state.json)
    after_backup=$(find /var/backups/nut-cli -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l)
    [[ $before_state == "$after_state" ]] || fail "second init changed state mtime"
    [[ $before_backup == "$after_backup" ]] || fail "second init created a backup"
    python3 - "$DIAG_DIR" <<'PY'
import json, sys
from pathlib import Path
diag = Path(sys.argv[1])
doc = json.loads((diag / "second-init.json").read_text())
assert not doc.get("files_applied"), doc
assert not doc.get("services_applied"), doc
def enters(path):
    out = {}
    for line in path.read_text().splitlines():
        name = line.split()[0]
        parts = dict(item.split("=", 1) for item in line.split()[1:])
        out[name] = parts.get("enter")
    return out
before = enters(diag / "pre-second/services.txt")
after = enters(diag / "pre-second/services.txt")
# Re-read live timestamps from the post-init snapshot taken below.
print("second init applied nothing")
PY
    snapshot_state "$DIAG_DIR/post-second"
    python3 - "$DIAG_DIR" <<'PY'
import sys
from pathlib import Path
diag = Path(sys.argv[1])
def enters(path):
    out = {}
    for line in path.read_text().splitlines():
        name = line.split()[0]
        parts = dict(item.split("=", 1) for item in line.split()[1:])
        out[name] = parts.get("enter")
    return out
if enters(diag / "pre-second/services.txt") != enters(diag / "post-second/services.txt"):
    sys.exit("second init restarted services")
print("service timestamps unchanged")
PY
}

compare_rollback() {
    python3 - "$SNAP_DIR" <<'PY'
import json, os, subprocess, sys
from pathlib import Path
dest = Path(sys.argv[1])
before = json.loads((dest / "files.json").read_text())
problems = []
for rec in before:
    path = Path(rec["path"])
    exists = path.exists()
    if rec["exists"] != exists:
        problems.append(f"{path} existence {exists} != {rec['exists']}")
        continue
    if not exists:
        continue
    st = path.stat()
    sha = subprocess.check_output(["sha256sum", str(path)], text=True).split()[0]
    if (st.st_uid, st.st_gid, st.st_mode & 0o7777, sha) != (
        rec["uid"], rec["gid"], rec["mode"], rec["sha256"]
    ):
        problems.append(
            f"{path} uid/gid/mode/hash {(st.st_uid, st.st_gid, st.st_mode & 0o7777, sha)} != {(rec['uid'], rec['gid'], rec['mode'], rec['sha256'])}"
        )
if problems:
    sys.exit("; ".join(problems))
print("rollback restored files")
PY
    dpkg-query -W -f='${Package} ${Version}\n' nut-client nut-server >"$SNAP_DIR/packages.after.txt"
    diff -u "$SNAP_DIR/packages.txt" "$SNAP_DIR/packages.after.txt" >/dev/null \
        || fail "package baseline changed; rollback must retain distribution packages"
    snapshot_state "$SNAP_DIR/after"
    python3 - "$SNAP_DIR" <<'PY'
import sys
from pathlib import Path
dest = Path(sys.argv[1])
def active_map(path):
    out = {}
    for line in path.read_text().splitlines():
        parts = dict(item.split("=", 1) for item in line.split()[1:])
        out[line.split()[0]] = (parts.get("active"), parts.get("enabled"))
    return out
before = active_map(dest / "services.txt")
after = active_map(dest / "after/services.txt")
if before != after:
    sys.exit(f"service state {after} != {before}")
print("service state restored")
PY
    if [[ -e $SNAP_DIR/nut-operators.group || -e $SNAP_DIR/after/nut-operators.group ]]; then
        diff -u "$SNAP_DIR/nut-operators.group" "$SNAP_DIR/after/nut-operators.group" \
            || fail "nut-operators membership delta was not restored"
    fi
    log "retained package baseline:"
    cat "$SNAP_DIR/packages.txt"
}

# The operator allowlist is proved three ways: the rendered upsd.users stanza
# carries exactly the beeper/test commands, the live operator credential is
# refused power commands, and an allowed command gets the unsupported reply.
# Dummy Mode implements no instant commands and may check support before ACLs;
# in that case the stanza assertion provides the authorization proof.
assert_operator_acl() {
    python3 - <<'PY'
from pathlib import Path
lines = Path("/etc/nut/upsd.users").read_text().splitlines()
in_operator, instcmds, stanzas = False, [], []
for line in lines:
    stripped = line.strip()
    if stripped.startswith("[") and stripped.endswith("]"):
        in_operator = stripped == "[nut-operator]"
        stanzas.append(stripped)
    elif in_operator and stripped.startswith("instcmds"):
        instcmds.append(stripped.split("=", 1)[1].strip())
if "[nut-operator]" not in stanzas:
    raise SystemExit("upsd.users has no [nut-operator] stanza")
want = {
    "beeper.disable",
    "beeper.enable",
    "beeper.mute",
    "test.battery.start.quick",
    "test.battery.stop",
}
if set(instcmds) != want or len(instcmds) != len(want):
    raise SystemExit(f"operator allowlist is not exactly the beeper/test set: {instcmds}")
blob = "\n".join(lines)
if "instcmds = ALL" in blob or "instcmds = load" in blob or "shutdown" in blob.split("[nut-operator]")[1]:
    raise SystemExit("operator stanza grants more than beeper/test commands")
print("operator allowlist is exactly beeper.* + quick test/stop")
PY
}

# Run one instant command as the operator and report the lowercased refusal.
operator_probe() {
    python3 "$ROOT/scripts/e2e_operator_probe.py" "$1"
}

run_actions_proofs() {
    log "operator allows beeper/test, refuses power commands"
    assert_operator_acl
    local lowered cmd out
    # Dummy Mode may reject unsupported commands before checking the ACL,
    # so the exact rendered stanza remains the authorization proof.
    lowered=$(operator_probe beeper.mute)
    [[ $lowered == *support* ]] || fail "beeper.mute refusal does not name support: $lowered"
    [[ $lowered != *denied* ]] || fail "beeper.mute was denied by the ACL: $lowered"
    log "beeper probe refused as unsupported: $lowered"
    # Either ACL or command-support checks refuse these power commands.
    # The stanza assertion covers the latter case.
    for cmd in load.off shutdown.return; do
        lowered=$(operator_probe "$cmd")
        if [[ $lowered == *denied* ]]; then
            log "operator refused $cmd: access denied"
        elif [[ $lowered == *support* ]]; then
            log "operator refused $cmd at the driver ($lowered); ACL proof is the stanza assertion"
        else
            fail "unexpected $cmd refusal: $lowered"
        fi
    done
    # The nut CLI speaks the same protocol: bare beeper reads, verbs reach the
    # driver refusal, notify test submits mail, and the test needs a terminal.
    "$NUT_BIN" --config "$CONFIG" beeper | grep -qi "not reported" \
        || fail "nut beeper did not report the missing beeper variable gracefully"
    if out=$("$NUT_BIN" --config "$CONFIG" beeper mute 2>&1); then
        fail "nut beeper mute unexpectedly succeeded: $out"
    fi
    [[ $out == *"does not support"* ]] || fail "nut beeper mute refusal is not the driver: $out"
    log "nut beeper mute reached the driver refusal"
    if out=$("$NUT_BIN" --config "$CONFIG" battery test --stop </dev/null 2>&1); then
        fail "nut battery test ran without a terminal: $out"
    fi
    [[ $out == *"interactive terminal"* ]] || fail "battery test refusal is not the terminal interlock: $out"
    log "nut battery test refuses to run without a terminal"
    : >"$MAIL_LOG"
    "$NUT_BIN" --config "$CONFIG" notify test | tee "$DIAG_DIR/notify-test.txt" | redact
    grep -qi "test notification" "$MAIL_LOG" \
        || fail "notify test did not submit mail through the hook"
    log "notify test submitted mail through the hook"
    "$NUT_BIN" --config "$CONFIG" notify | tee "$DIAG_DIR/notify.txt" | redact | grep -qi "operator@example.invalid" \
        || fail "nut notify did not show the recipients"
    log "nut notify shows recipients"
}

run_isolated_yes() {
    log "isolated root init --yes fixture"
    "$NUT_BIN" --json --config "$CONFIG" init --yes | tee "$DIAG_DIR/isolated-yes.json" | redact
    [[ -e /etc/nut-cli/nut.toml ]] || fail "isolated init did not install host config"
    local stamp
    stamp=$(json_get backup_stamp <"$DIAG_DIR/isolated-yes.json")
    "$NUT_BIN" --config "$CONFIG" rollback --yes "$stamp" | tee "$DIAG_DIR/isolated-rollback.txt" | redact
    compare_rollback
}

run_hash_bound_init() {
    log "authoritative dry-run"
    local preview hash
    preview=$("$NUT_BIN" --json --config "$CONFIG" init --dry-run)
    printf '%s\n' "$preview" >"$DIAG_DIR/dry-run.json"
    python3 - "$DIAG_DIR/dry-run.json" <<'PY'
import json,sys
doc=json.load(open(sys.argv[1]))
assert doc["complete"] is True, doc
assert doc.get("hash"), doc
assert doc["empty"] is False
print("dry-run complete")
PY
    hash=$(printf '%s' "$preview" | json_get hash)
    log "hash-bound init $hash"
    "$NUT_BIN" --json --config "$CONFIG" init --yes --expect-plan "$hash" \
        | tee "$DIAG_DIR/init.json" | redact
    python3 - "$DIAG_DIR/init.json" <<'PY'
import json,sys
doc=json.load(open(sys.argv[1]))
assert doc["state_finalized"] is True
assert doc["verification"]["failed"] == 0
print("hash-bound init finalized")
PY
    poll_until "driver active" systemctl is-active 'nut-driver@dummy.service'
    poll_until "server active" systemctl is-active nut-server.service
    poll_until "monitor active" systemctl is-active nut-monitor.service
}

main() {
    if [[ ${1:-} == -h || ${1:-} == --help ]]; then
        usage
        exit 0
    fi
    if [[ ${1:-} == --lint ]]; then
        lint_only
        exit 0
    fi
    [[ $(id -u) -eq 0 ]] || fail "e2e must run as root on the disposable runner"
    if [[ -z ${NUT_E2E_INNER:-} ]]; then
        command -v timeout >/dev/null
        export NUT_E2E_INNER=1
        exec timeout --signal=TERM --kill-after=30s "$TIMEOUT_SECS" "$0" "$@"
    fi
    mkdir -p "$DIAG_DIR"
    trap on_exit EXIT

    install_nut_packages
    install_stubs
    prepare_runner_fstab
    install_dummy_source
    cd "$ROOT"
    install_cli
    SNAP_DIR=$DIAG_DIR/pre-init
    snapshot_state "$SNAP_DIR"
    printf 'NUT %s\n' "$(dpkg-query -W -f='${Version}' nut-server)" | tee "$DIAG_DIR/nut-version.txt"

    run_isolated_yes
    run_hash_bound_init
    assert_acl
    doctor_ok
    second_init_noop
    run_actions_proofs

    local stamp
    stamp=$(json_get backup_stamp <"$DIAG_DIR/init.json")
    [[ -n $stamp ]] || fail "missing backup stamp"

    log "inject OB DISCHRG"
    write_dummy_dev "$DEV_OB"
    poll_until "status OB" status_has OB
    poll_until "events on-battery" events_reflect "on battery|still on battery"

    log "restore OL CHRG"
    write_dummy_dev "$DEV_OL"
    poll_until "status OL" status_has OL
    poll_until "events recovered" events_reflect "power restored|on line power|on line"

    log "rollback $stamp"
    "$NUT_BIN" --config "$CONFIG" rollback --yes "$stamp" | tee "$DIAG_DIR/rollback.txt" | redact
    compare_rollback

    if [[ -s $SHUTDOWN_LOG ]]; then
        fail "shutdown helper was invoked:\n$(cat "$SHUTDOWN_LOG")"
    fi
    log "shutdown helper was not invoked"
    log "PASS"
}

main "$@"
