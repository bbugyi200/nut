use std::path::Path;
use std::process::Command;

use nut_cli::config::{HostConfig, resolve};
use nut_cli::host::SystemHost;
use nut_cli::profiles::load_catalog;
use nut_cli::render::render_ups_conf;

fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn dummy_host_fixture_resolves_and_renders_without_usb() {
    let text = include_str!("fixtures/e2e/host.toml");
    let config = HostConfig::parse(text).expect("e2e host.toml parses");
    let host = SystemHost;
    let catalog = load_catalog(&host).expect("catalog");
    let resolved = resolve("/e2e.toml".into(), &config, &catalog).expect("dummy resolves");
    assert_eq!(resolved.driver.value, "dummy-ups");
    assert_eq!(resolved.port.value, "/var/lib/nut/e2e.dev");
    assert_eq!(resolved.ups_name, "dummy");
    assert_eq!(resolved.shutdown.early_after.as_secs(), 30 * 60);
    assert_eq!(resolved.shutdown.poweroff_wait.as_secs(), 3 * 3600);
    let rendered = render_ups_conf(&resolved).expect("render dummy ups.conf");
    assert!(rendered.contains("driver = dummy-ups"));
    assert!(rendered.contains("port = /var/lib/nut/e2e.dev"));
    assert!(!rendered.contains("vendorid"));
    assert!(!rendered.contains("productid"));
    assert!(!rendered.contains("offdelay"), "dummy-ups rejects offdelay");
    assert!(!rendered.contains("ondelay"), "dummy-ups rejects ondelay");
}

#[test]
fn dummy_dev_fixtures_carry_required_variables() {
    let online = include_str!("fixtures/e2e/dummy.dev");
    for line in [
        "ups.status: OL CHRG",
        "battery.charge: 100",
        "battery.runtime: 7200",
        "ups.delay.shutdown: 60",
        "ups.delay.start: 0",
        "ups.test.result: OK",
    ] {
        assert!(online.contains(line), "missing {line}");
    }
    let onbatt = include_str!("fixtures/e2e/dummy-ob.dev");
    assert!(onbatt.contains("ups.status: OB DISCHRG"));
}

#[test]
fn e2e_script_lints_without_nut() {
    let script = repo_root().join("scripts/e2e_dummy.sh");
    let syntax = Command::new("bash")
        .args(["-n"])
        .arg(&script)
        .output()
        .expect("bash -n");
    assert!(
        syntax.status.success(),
        "{}",
        String::from_utf8_lossy(&syntax.stderr)
    );
    let lint = Command::new("bash")
        .arg(&script)
        .arg("--lint")
        .output()
        .expect("e2e --lint");
    assert!(
        lint.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&lint.stdout),
        String::from_utf8_lossy(&lint.stderr)
    );
}

#[test]
fn e2e_stubs_are_posix_shell() {
    for name in ["shutdown", "sendmail"] {
        let path = repo_root().join("scripts/e2e_stubs").join(name);
        let output = Command::new("bash")
            .args(["-n"])
            .arg(&path)
            .output()
            .expect("bash -n stub");
        assert!(
            output.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = std::fs::read_to_string(&path).expect("read stub");
        assert!(text.contains("Disposable CI stub"));
    }
}

#[test]
fn operator_probe_keeps_credentials_in_files_and_refuses_real_drivers() {
    use nut_cli::fake_upsd::{FakeUpsdFixture, FakeUser, serve_client};
    use std::collections::BTreeMap;
    use std::net::TcpListener;
    use std::thread;

    let secret_dir = tempfile::tempdir().unwrap();
    let secret_path = secret_dir.path().join("operator.secret");
    let secret = "probe secret with \"quotes\" and \\slashes";
    std::fs::write(&secret_path, format!("{secret}\n")).unwrap();
    for (driver, command, commands, expected) in [
        (
            "dummy-ups",
            "beeper.mute",
            vec![],
            Some("err cmd-not-supported"),
        ),
        (
            "dummy-ups",
            "load.off",
            vec!["load.off"],
            Some("err access-denied"),
        ),
        (
            "dummy-ups",
            "shutdown.return",
            vec!["shutdown.return"],
            Some("err access-denied"),
        ),
        ("usbhid-ups", "beeper.mute", vec![], None),
        ("dummy-ups", "beeper.mute", vec!["beeper.mute"], None),
    ] {
        let mut fixture = FakeUpsdFixture {
            ups_name: "dummy".into(),
            description: "Disposable probe test".into(),
            variables: BTreeMap::from([("driver.name".into(), driver.into())]),
            commands: commands.into_iter().map(String::from).collect(),
            users: BTreeMap::from([(
                "nut-operator".into(),
                FakeUser {
                    password: secret.into(),
                    instcmds: vec!["beeper.mute".into()],
                },
            )]),
            ..Default::default()
        };
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            serve_client(stream, &mut fixture).unwrap();
        });
        let output = Command::new("python3")
            .args(["-B", "-c", "import sys; from pathlib import Path; sys.path.insert(0, 'scripts'); from e2e_operator_probe import probe; print(probe(sys.argv[1], Path(sys.argv[2]), int(sys.argv[3])))"])
            .arg(command)
            .arg(&secret_path)
            .arg(port.to_string())
            .current_dir(repo_root())
            .output()
            .unwrap();
        server.join().unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(!stdout.contains(secret));
        assert!(!stderr.contains(secret));
        assert_eq!(output.status.success(), expected.is_some(), "{stderr}");
        if let Some(expected) = expected {
            assert_eq!(stdout.trim(), expected);
        }
    }
}
