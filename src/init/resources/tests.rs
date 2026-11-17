use super::*;
use crate::host::{FakeHost, HostEffect};
use crate::init::{BeginRequest, begin, recover, recover_with_observations};

fn out(status: i32, stdout: &str) -> ProcessOutput {
    ProcessOutput {
        status: Some(status),
        stdout: stdout.as_bytes().into(),
        stderr: vec![],
    }
}
fn host() -> FakeHost {
    FakeHost::default()
        .with_euid(0)
        .with_program_args("getent", ["group", "nut"], out(0, "nut:x:100:nut\n"))
        .with_program_args(
            "getent",
            ["group", "nut-operators"],
            out(0, "nut-operators:x:101:alice\n"),
        )
}
fn request(resource: &PreparedResource) -> BeginRequest {
    let mut targets = resource.backup_targets();
    targets.push("/var/lib/nut-cli/state.json".into());
    BeginRequest {
        plan_hash: "a".repeat(64),
        targets,
        services: vec![],
        membership_additions: match &resource.resource.0 {
            Action::Membership { user, group } => vec![crate::init::MembershipAddition {
                user: user.clone(),
                group: group.clone(),
            }],
            _ => vec![],
        },
        created_groups: match &resource.resource.0 {
            Action::OperatorsGroup => vec!["nut-operators".into()],
            _ => vec![],
        },
        operations: vec![resource.intent()],
    }
}
fn prepare(host: &FakeHost, resource: Resource) -> PreparedResource {
    resource.prepare(host, "resource:test").unwrap().unwrap()
}
fn file(bytes: &[u8]) -> Resource {
    Resource::file(
        "/etc/nut/upsd.users".into(),
        bytes.into(),
        FileMetadata::file(0, 100, 0o640),
    )
    .unwrap()
}
fn effects(host: &FakeHost) -> Vec<HostEffect> {
    host.effects()
        .into_iter()
        .filter(|e| !matches!(e, HostEffect::Run { .. } | HostEffect::ObserveUps { .. }))
        .collect()
}

#[test]
fn atomic_file_modes_noop_and_drift_refusal() {
    let host = host().with_file("/etc/nut/upsd.users", b"old secret");
    let resource = prepare(&host, file(b"new secret"));
    let tx = begin(&host, &request(&resource)).unwrap().unwrap();
    apply_resource(&host, &tx, &resource).unwrap();
    assert_eq!(
        host.read_protected(Path::new("/etc/nut/upsd.users"))
            .unwrap()
            .unwrap(),
        b"new secret"
    );
    assert_eq!(
        host.metadata(Path::new("/etc/nut/upsd.users")).unwrap(),
        Some(FileMetadata::file(0, 100, 0o640))
    );
    let before = host.effects();
    apply_resource(&host, &tx, &resource).unwrap();
    assert_eq!(before, host.effects());
    assert!(
        file(b"new secret")
            .prepare(&host, "next")
            .unwrap()
            .is_none()
    );
    host.replace_file_atomic(
        Path::new("/etc/nut/upsd.users"),
        b"foreign edit",
        &FileMetadata::file(0, 100, 0o640),
    )
    .unwrap();
    assert!(apply_resource(&host, &tx, &resource).is_err());
}

#[test]
fn restart_after_publication_does_not_repeat_file_effect() {
    let host = host().with_file("/etc/nut/upsd.users", b"original");
    let resource = prepare(&host, file(b"new credential"));
    let tx = begin(&host, &request(&resource)).unwrap().unwrap();
    // Started journal + target publication succeed; completion publication fails.
    host.fail_after("replace_file", 2, "crash before completion");
    assert!(apply_resource(&host, &tx, &resource).is_err());
    let restarted = host.snapshot_filesystem();
    drop(tx);
    let report = recover(&restarted).unwrap().unwrap();
    assert_eq!(
        report.operations[0].observed,
        crate::init::ReconciledOperation::After
    );
    let resumed = resume_resource(
        &restarted,
        &report.transaction,
        "resource:test",
        file(b"new credential"),
    )
    .unwrap();
    apply_resource(&restarted, &report.transaction, &resumed).unwrap();
    assert!(!restarted.effects().iter().any(|e| matches!(e, HostEffect::ReplaceFile { path, .. } if path == Path::new("/etc/nut/upsd.users"))));
    assert_eq!(
        transaction::original_file(
            &restarted,
            &report.transaction,
            Path::new("/etc/nut/upsd.users")
        )
        .unwrap()
        .unwrap()
        .0,
        b"original"
    );
}

#[test]
fn failure_before_target_preserves_original_and_retries() {
    let host = host().with_file("/etc/nut/upsd.users", b"original");
    let resource = prepare(&host, file(b"changed"));
    let tx = begin(&host, &request(&resource)).unwrap().unwrap();
    host.fail_after("replace_file", 1, "target failure");
    assert!(apply_resource(&host, &tx, &resource).is_err());
    assert_eq!(
        host.read_protected(Path::new("/etc/nut/upsd.users"))
            .unwrap()
            .unwrap(),
        b"original"
    );
    apply_resource(&host, &tx, &resource).unwrap();
}

#[test]
fn deferred_secret_binding_is_once_only_and_requires_backup() {
    let host = host();
    let id = "file:operator";
    let request = BeginRequest {
        plan_hash: "b".repeat(64),
        targets: vec![
            "/etc/nut-cli/operator.secret".into(),
            "/var/lib/nut-cli/state.json".into(),
        ],
        operations: vec![reserve_resource(id)],
        services: vec![],
        membership_additions: vec![],
        created_groups: vec![],
    };
    let tx = begin(&host, &request).unwrap().unwrap();
    let secret =
        crate::init::materialize_secret(&host, &tx, crate::init::SecretPurpose::Operator, 16)
            .unwrap();
    let mut bytes = secret.expose().to_vec();
    bytes.push(b'\n');
    let mut resource = Resource::file(
        "/etc/nut-cli/operator.secret".into(),
        bytes,
        FileMetadata::file(0, 101, 0o640),
    )
    .unwrap()
    .prepare(&host, id)
    .unwrap()
    .unwrap();
    bind_resource(&host, &tx, &mut resource).unwrap();
    apply_resource(&host, &tx, &resource).unwrap();
    let before = effects(&host);
    apply_resource(&host, &tx, &resource).unwrap();
    assert_eq!(before, effects(&host));
    let mut changed = Resource::file(
        "/etc/nut-cli/operator.secret".into(),
        b"second secret\n".into(),
        FileMetadata::file(0, 101, 0o640),
    )
    .unwrap()
    .prepare(&host, id)
    .unwrap()
    .unwrap();
    assert!(bind_resource(&host, &tx, &mut changed).is_err());
    let mut wrong = file(b"unbacked").prepare(&host, id).unwrap().unwrap();
    assert!(bind_resource(&host, &tx, &mut wrong).is_err());
    let restarted = host.snapshot_filesystem();
    let fingerprints = BTreeMap::from([(id.into(), resource.observe(&restarted).unwrap())]);
    let report = recover_with_observations(&restarted, &fingerprints)
        .unwrap()
        .unwrap();
    assert_eq!(
        report.operations[0].observed,
        crate::init::ReconciledOperation::After
    );
}

#[test]
fn membership_is_additive_and_verified_not_just_exit_zero() {
    let mut host = host()
        .with_program_args("id", ["--groups", "--", "alice"], out(0, "1000 27 101"))
        .with_program("usermod", out(0, ""));
    for _ in 0..2 {
        host = host.queue_program("id", ["--groups", "--", "alice"], out(0, "1000 27"));
    }
    let resource = prepare(
        &host,
        Resource::membership("alice", "nut-operators").unwrap(),
    );
    let tx = begin(&host, &request(&resource)).unwrap().unwrap();
    apply_resource(&host, &tx, &resource).unwrap();
    let calls = host.effects();
    assert!(calls.iter().any(|e| matches!(e, HostEffect::Run { program, args } if program == "usermod" && args == &["--append", "--groups", "nut-operators", "--", "alice"].map(OsString::from))));
    let count = calls
        .iter()
        .filter(|e| matches!(e, HostEffect::Run { program, .. } if program == "usermod"))
        .count();
    apply_resource(&host, &tx, &resource).unwrap();
    assert_eq!(
        count,
        host.effects()
            .iter()
            .filter(|e| matches!(e, HostEffect::Run { program, .. } if program == "usermod"))
            .count()
    );
    for user in ["-root", "a,b", "a b", "a\n", "a;id"] {
        assert!(Resource::membership(user, "nut-operators").is_err());
    }
    assert!(Resource::membership("alice", "wheel").is_err());
}

#[test]
fn successful_command_without_membership_change_stays_started() {
    let host = host()
        .with_program_args("id", ["--groups", "--", "alice"], out(0, "1000 27"))
        .with_program("usermod", out(0, ""));
    let resource = prepare(
        &host,
        Resource::membership("alice", "nut-operators").unwrap(),
    );
    let tx = begin(&host, &request(&resource)).unwrap().unwrap();
    assert!(apply_resource(&host, &tx, &resource).is_err());
    assert_eq!(
        transaction::operation_record(&host, &tx, &resource.intent.id)
            .unwrap()
            .1,
        CheckpointState::Started
    );
}

fn package_host(simulation: &str) -> FakeHost {
    host()
        .with_program("dpkg-query", out(1, ""))
        .with_program("apt-get", out(0, simulation))
}
fn requested() -> BTreeMap<String, String> {
    BTreeMap::from([("nut-client".into(), "2.8.1-1".into())])
}
const SIM: &str =
    "Inst nut-client (2.8.1-1 Ubuntu [amd64])\nConf nut-client (2.8.1-1 Ubuntu [amd64])\n";
fn query() -> [&'static str; 4] {
    [
        "-W",
        "-f=${db:Status-Status} ${Version}",
        "--",
        "nut-client",
    ]
}

#[test]
fn package_solution_rejects_upgrades_removals_and_option_injection() {
    for simulation in [
        "Inst nut-client [2.8.0] (2.8.1-1 Ubuntu)\n",
        "Remv nut-server [2.8.1]\n",
        "Conf unrelated (1.0 Ubuntu)\n",
        "Inst nut-client (-oBad Ubuntu)\n",
    ] {
        let host = package_host(simulation);
        assert!(Resource::packages(&host, &requested()).is_err());
        assert!(effects(&host).is_empty());
    }
    let host = package_host(SIM).with_program("dpkg-query", out(0, "installed 2.8.1-1"));
    assert!(Resource::packages(&host, &requested()).is_err());
    assert!(Resource::packages(&host, &BTreeMap::from([("--bad".into(), "1".into())])).is_err());
}

#[test]
fn missing_only_install_is_guarded_pinned_and_reapply_has_no_effects() {
    let mut host = package_host(SIM);
    // Constructor probes twice, prepare once, apply before and install once.
    for _ in 0..5 {
        host = host.queue_program("dpkg-query", query(), out(1, ""));
    }
    host = host.with_program("dpkg-query", out(0, "installed 2.8.1-1"));
    let resource = prepare(&host, Resource::packages(&host, &requested()).unwrap());
    let tx = begin(&host, &request(&resource)).unwrap().unwrap();
    apply_resource(&host, &tx, &resource).unwrap();
    let trace = host.effects();
    let guard = trace
        .iter()
        .position(
            |e| matches!(e, HostEffect::ReplaceFile { path, .. } if path == Path::new(POLICY)),
        )
        .unwrap();
    let apt = trace.iter().position(|e| matches!(e, HostEffect::Run { program, args } if program == "apt-get" && args.contains(&"--assume-yes".into()))).unwrap();
    let restore = trace
        .iter()
        .position(|e| matches!(e, HostEffect::Remove { path } if path == Path::new(POLICY)))
        .unwrap();
    assert!(guard < apt && apt < restore);
    assert!(
        matches!(&trace[apt], HostEffect::Run { args, .. } if args.contains(&"--no-upgrade".into()) && args.contains(&"--no-remove".into()) && args.contains(&"nut-client=2.8.1-1".into()))
    );
    let writes = effects(&host);
    let installs = trace.iter().filter(|e| matches!(e, HostEffect::Run { program, args } if program == "apt-get" && args.contains(&"--assume-yes".into()))).count();
    apply_resource(&host, &tx, &resource).unwrap();
    assert_eq!(writes, effects(&host));
    assert_eq!(installs, host.effects().iter().filter(|e| matches!(e, HostEffect::Run { program, args } if program == "apt-get" && args.contains(&"--assume-yes".into()))).count());
}

#[test]
fn apt_failure_preserves_an_existing_policy_and_hides_output() {
    let bytes = b"#!/bin/sh\n# administrator policy\nexit 101\n";
    let host = package_host(SIM)
        .with_file(POLICY, bytes)
        .with_metadata(POLICY, FileMetadata::file(0, 0, 0o755))
        .with_program(POLICY, out(101, ""))
        .with_program_args(
            "apt-get",
            apt_args(&requested(), false),
            out(100, "SENTINEL_SECRET"),
        );
    let resource = prepare(&host, Resource::packages(&host, &requested()).unwrap());
    let tx = begin(&host, &request(&resource)).unwrap().unwrap();
    let error = apply_resource(&host, &tx, &resource)
        .unwrap_err()
        .to_string();
    assert!(!error.contains("SENTINEL_SECRET"));
    assert_eq!(
        host.read_protected(Path::new(POLICY)).unwrap().unwrap(),
        bytes
    );
    assert!(!host.effects().iter().any(|e| matches!(e, HostEffect::ReplaceFile { path, .. } | HostEffect::Remove { path } if path == Path::new(POLICY))));
}

#[test]
fn apt_failure_removes_own_policy_and_crashed_guard_can_be_restored() {
    let host = package_host(SIM).with_program_args(
        "apt-get",
        apt_args(&requested(), false),
        out(100, "failed"),
    );
    let resource = prepare(&host, Resource::packages(&host, &requested()).unwrap());
    let tx = begin(&host, &request(&resource)).unwrap().unwrap();
    assert!(apply_resource(&host, &tx, &resource).is_err());
    assert!(host.read_protected(Path::new(POLICY)).unwrap().is_none());
    suppress_package_starts(&host, &tx).unwrap();
    let restarted = host.snapshot_filesystem();
    drop(tx);
    let report = recover(&restarted).unwrap().unwrap();
    restore_package_policy(&restarted, &report.transaction).unwrap();
    assert!(
        restarted
            .read_protected(Path::new(POLICY))
            .unwrap()
            .is_none()
    );
}

#[test]
fn permissive_policy_and_changed_dependency_solution_fail_before_install() {
    let host = package_host(SIM)
        .with_file(POLICY, b"#!/bin/sh\nexit 0\n")
        .with_metadata(POLICY, FileMetadata::file(0, 0, 0o755))
        .with_program(POLICY, out(0, ""));
    let resource = prepare(&host, Resource::packages(&host, &requested()).unwrap());
    let tx = begin(&host, &request(&resource)).unwrap().unwrap();
    assert!(apply_resource(&host, &tx, &resource).is_err());
    assert!(!host.effects().iter().any(|e| matches!(e, HostEffect::Run { program, args } if program == "apt-get" && args.contains(&"--assume-yes".into()))));
    let host = package_host("Inst nut-client (2.8.1-1 Ubuntu)\nInst surprise (1.0 Ubuntu)\n")
        .queue_program("apt-get", apt_args(&requested(), true), out(0, SIM));
    let resource = prepare(&host, Resource::packages(&host, &requested()).unwrap());
    let tx = begin(&host, &request(&resource)).unwrap().unwrap();
    assert!(apply_resource(&host, &tx, &resource).is_err());
    assert!(host.read_protected(Path::new(POLICY)).unwrap().is_none());
}

#[test]
fn fstab_preserves_bytes_and_metadata_without_remount_and_checks_reported_errors() {
    let old = b"# keep\nUUID=a\t/data\text4\tdefaults  0 2 # keep comment\nUUID=b /other ext4 defaults 0 2\n";
    let expected = b"# keep\nUUID=a\t/data\text4\tdefaults,nofail  0 2 # keep comment\nUUID=b /other ext4 defaults 0 2\n";
    assert_eq!(nofail_candidate(old, &["/data".into()]).unwrap(), expected);
    assert_eq!(
        nofail_candidate(expected, &["/data".into()]).unwrap(),
        expected
    );
    assert!(nofail_candidate(old, &["/missing".into()]).is_err());
    assert!(
        nofail_candidate(
            b"a /data ext4 defaults 0 2\nb /data ext4 defaults 0 2\n",
            &["/data".into()]
        )
        .is_err()
    );
    for (status, summary, succeeds) in [
        (0, "0 parse errors, 0 errors, 1 warning", true),
        (0, "1 parse error, 0 errors", false),
        (0, "0 parse errors, 2 errors", false),
        (1, "bad", false),
    ] {
        let host = host()
            .with_file(FSTAB_PATH, old)
            .with_metadata(FSTAB_PATH, FileMetadata::file(0, 9, 0o640))
            .with_program("findmnt", out(status, summary));
        let resource = prepare(
            &host,
            Resource::fstab(old, &["/data".into()], FileMetadata::file(0, 9, 0o640)).unwrap(),
        );
        let tx = begin(&host, &request(&resource)).unwrap().unwrap();
        assert_eq!(apply_resource(&host, &tx, &resource).is_ok(), succeeds);
        assert_eq!(
            host.read_protected(Path::new(FSTAB_PATH)).unwrap().unwrap(),
            if succeeds {
                expected.as_slice()
            } else {
                old.as_slice()
            }
        );
        assert_eq!(
            host.metadata(Path::new(FSTAB_PATH)).unwrap().unwrap(),
            FileMetadata::file(0, 9, 0o640)
        );
        assert!(
            !host
                .effects()
                .iter()
                .any(|e| matches!(e, HostEffect::Run { program, .. } if program == "mount"))
        );
    }
}

fn usb_host() -> FakeHost {
    host()
        .with_file("/sys/bus/usb/devices/1-4/idVendor", b"0764")
        .with_file("/sys/bus/usb/devices/1-4/idProduct", b"0601")
        .with_file("/sys/bus/usb/devices/1-4/serial", b"chosen")
        .with_file("/sys/bus/usb/devices/1-4/busnum", b"1")
        .with_file("/sys/bus/usb/devices/1-4/devnum", b"4")
        .with_metadata("/dev/bus/usb/001/004", FileMetadata::file(0, 0, 0o660))
        .with_program("udevadm", out(0, ""))
}
fn device() -> Resource {
    Resource::device(
        "/sys/bus/usb/devices/1-4".into(),
        "0764",
        "0601",
        Some("chosen".into()),
        100,
        false,
    )
    .unwrap()
    .unwrap()
}

#[test]
fn udev_targets_one_device_and_requires_verified_group_with_dummy_skip() {
    let host = usb_host();
    let resource = prepare(&host, device());
    let tx = begin(&host, &request(&resource)).unwrap().unwrap();
    // Exit zero alone cannot prove that a udev rule fixed the node.
    assert!(apply_resource(&host, &tx, &resource).is_err());
    assert!(host.effects().iter().any(|e| matches!(e, HostEffect::Run { program, args } if program == "udevadm" && args == &["trigger", "--action=change", "--", "/sys/bus/usb/devices/1-4"].map(OsString::from))));
    host.set_metadata(
        Path::new("/dev/bus/usb/001/004"),
        &FileMetadata::file(0, 100, 0o660),
    )
    .unwrap();
    let count = host.effects().len();
    apply_resource(&host, &tx, &resource).unwrap();
    assert!(
        !host.effects()[count..]
            .iter()
            .any(|e| matches!(e, HostEffect::Run { program, .. } if program == "udevadm"))
    );
    assert!(device().prepare(&host, "next").unwrap().is_none());
    assert!(
        Resource::device("--bad".into(), "bad", "bad", None, 100, true)
            .unwrap()
            .is_none()
    );
    assert!(
        Resource::device(
            "/sys/bus/usb/devices/../../bad".into(),
            "0764",
            "0601",
            None,
            100,
            false
        )
        .is_err()
    );
}

#[test]
fn legacy_retirement_requires_exact_known_body_and_live_references() {
    let old = include_bytes!("../../../tests/fixtures/hosts/athena/deployed/athena-upssched-cmd");
    let hook = String::from_utf8_lossy(old)
        .replace("ATHENA_UPSSCHED_TEST_PATH", "NUT_UPSSCHED_TEST_PATH")
        .replace("athena-upssched", "nut-upssched")
        .into_bytes();
    let upssched = format!("CMDSCRIPT {LIVE_HOOK}\n").into_bytes();
    for replaced in [false, true] {
        let mut host = host()
            .with_file(LEGACY_HOOK, old.as_slice())
            .with_file("/etc/nut/upsmon.conf", b"NOTIFYCMD /usr/sbin/upssched\n");
        if replaced {
            host = host
                .with_file(LIVE_HOOK, hook.clone())
                .with_metadata(LIVE_HOOK, FileMetadata::file(0, 0, 0o755))
                .with_file(LIVE_UPSSCHED_CONF, upssched.clone());
        }
        let resource = prepare(
            &host,
            Resource::retire_legacy(hook.clone(), upssched.clone()),
        );
        let tx = begin(&host, &request(&resource)).unwrap().unwrap();
        assert_eq!(apply_resource(&host, &tx, &resource).is_ok(), replaced);
        assert_eq!(
            host.read_protected(Path::new(LEGACY_HOOK))
                .unwrap()
                .is_none(),
            replaced
        );
        assert_eq!(
            transaction::original_file(&host, &tx, Path::new(LEGACY_HOOK))
                .unwrap()
                .unwrap()
                .0,
            old
        );
    }
    assert!(!recognized_legacy(
        b"#!/bin/sh\n# ATHENA_UPSSCHED_TEST_PATH athena-upssched\necho unrelated",
        &hook
    ));
}

#[test]
fn group_creation_is_verified_and_idempotent() {
    let host = host()
        .with_program("groupadd", out(0, ""))
        .queue_program("getent", ["group", "nut-operators"], out(2, ""))
        .queue_program("getent", ["group", "nut-operators"], out(2, ""));
    let resource = prepare(&host, Resource::operators_group());
    let tx = begin(&host, &request(&resource)).unwrap().unwrap();
    apply_resource(&host, &tx, &resource).unwrap();
    let count = host
        .effects()
        .iter()
        .filter(|e| matches!(e, HostEffect::Run { program, .. } if program == "groupadd"))
        .count();
    assert_eq!(count, 1);
    apply_resource(&host, &tx, &resource).unwrap();
    assert_eq!(
        count,
        host.effects()
            .iter()
            .filter(|e| matches!(e, HostEffect::Run { program, .. } if program == "groupadd"))
            .count()
    );
}

#[test]
fn fstab_retry_removes_interrupted_validation_candidate() {
    let old = b"a /data ext4 defaults 0 2\n";
    let host = host()
        .with_file(FSTAB_PATH, old)
        .with_program("findmnt", out(0, "0 errors"));
    let resource = prepare(
        &host,
        Resource::fstab(old, &["/data".into()], FileMetadata::file(0, 0, 0o644)).unwrap(),
    );
    let tx = begin(&host, &request(&resource)).unwrap().unwrap();
    host.fail_after("remove_protected", 0, "interrupted cleanup");
    assert!(apply_resource(&host, &tx, &resource).is_err());
    let candidate = PathBuf::from(format!("/etc/.nut-cli-fstab-{}", tx.stamp));
    assert!(host.read_protected(&candidate).unwrap().is_some());
    let before = host.effects();
    apply_resource(&host, &tx, &resource).unwrap();
    assert!(host.read_protected(&candidate).unwrap().is_none());
    assert!(!host.effects()[before.len()..].iter().any(
        |e| matches!(e, HostEffect::ReplaceFile { path, .. } if path == Path::new(FSTAB_PATH))
    ));
}

#[test]
fn dependency_closure_is_explicit_and_installed_dependencies_are_refused() {
    let simulation = format!("{SIM}Inst libups (1.0 Ubuntu)\nConf libups (1.0 Ubuntu)\n");
    let host = package_host(&simulation);
    let resource = Resource::packages(&host, &requested()).unwrap();
    assert_eq!(
        resource.package_versions().unwrap().get("libups").unwrap(),
        "1.0"
    );
    let host = host.with_program_args(
        "dpkg-query",
        ["-W", "-f=${db:Status-Status} ${Version}", "--", "libups"],
        out(0, "installed 0.9"),
    );
    assert!(Resource::packages(&host, &requested()).is_err());
}
