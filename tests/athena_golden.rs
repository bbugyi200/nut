use std::fs;
use std::path::PathBuf;
use std::process::Command;

use nut_cli::config::{HostConfig, resolve};
use nut_cli::host::SystemHost;
use nut_cli::profiles::load_catalog;
use nut_cli::render::{self, HOOK_NAME, active_lines, normalize_deployed_active_lines};

fn rendered_athena() -> nut_cli::render::RenderedFiles {
    let config = HostConfig::parse(include_str!("fixtures/hosts/athena/nut.toml")).unwrap();
    let catalog = load_catalog(&SystemHost).unwrap();
    let resolved = resolve(
        PathBuf::from("tests/fixtures/hosts/athena/nut.toml"),
        &config,
        &catalog,
    )
    .unwrap();
    render::render(&resolved).unwrap()
}

#[test]
fn athena_active_lines_match_deployed_files_modulo_documented_deltas() {
    let files = rendered_athena();
    let cases = [
        ("nut.conf", "deployed/nut.conf"),
        ("ups.conf", "deployed/ups.conf"),
        ("upsd.users", "deployed/upsd.users"),
        ("upsmon.conf", "deployed/upsmon.conf"),
        ("upssched.conf", "deployed/upssched.conf"),
        (HOOK_NAME, "deployed/athena-upssched-cmd"),
    ];
    for (name, deployed) in cases {
        let rendered = files.get(name).unwrap();
        let live = fs::read_to_string(format!("tests/fixtures/hosts/athena/{deployed}")).unwrap();
        let expected = normalize_deployed_active_lines(name, &live);
        let actual = active_lines(rendered);
        assert_eq!(
            actual, expected,
            "{name} active lines drifted from the redacted athena golden"
        );
    }
}

#[test]
fn rendered_hook_matches_the_shellchecked_golden() {
    let files = rendered_athena();
    let expected =
        fs::read_to_string("tests/fixtures/hosts/athena/expected/nut-upssched-cmd").unwrap();
    assert_eq!(files.hook, expected);
}

#[test]
fn rendered_files_match_snapshots() {
    let files = rendered_athena();
    insta::assert_snapshot!("athena_nut_conf", files.nut_conf.as_str());
    insta::assert_snapshot!("athena_ups_conf", files.ups_conf.as_str());
    insta::assert_snapshot!("athena_upsd_users", files.upsd_users.as_str());
    insta::assert_snapshot!("athena_upsmon_conf", files.upsmon_conf.as_str());
    insta::assert_snapshot!("athena_upssched_conf", files.upssched_conf.as_str());
    insta::assert_snapshot!("athena_hook", files.hook.as_str());
}

#[test]
fn config_render_writes_placeholder_secrets_and_no_live_passwords() {
    let output = Command::new(env!("CARGO_BIN_EXE_nut"))
        .args([
            "--config",
            "tests/fixtures/hosts/athena/nut.toml",
            "config",
            "render",
        ])
        .output()
        .expect("run nut config render");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("@UPSMON_PASSWORD@"));
    assert!(stdout.contains("@OPERATOR_PASSWORD@"));
    assert!(stdout.contains("nut-operator"));
    assert!(stdout.contains("CMDSCRIPT /usr/local/sbin/nut-upssched-cmd"));
    assert!(!stdout.contains("bbugyi"));
    assert!(!stdout.contains("password = \""));
}

#[test]
fn config_new_writes_a_parseable_scaffold() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nut.toml");
    let output = Command::new(env!("CARGO_BIN_EXE_nut"))
        .args(["config", "new", "-o", path.to_str().unwrap()])
        .output()
        .expect("run nut config new");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let contents = fs::read_to_string(&path).unwrap();
    assert!(contents.contains("schema = 1"));
    assert!(contents.contains("example.invalid"));
    HostConfig::parse(&contents).unwrap();
}

#[test]
fn config_diff_marks_missing_live_files_without_failing() {
    let output = Command::new(env!("CARGO_BIN_EXE_nut"))
        .args([
            "--config",
            "tests/fixtures/hosts/athena/nut.toml",
            "config",
            "diff",
        ])
        .output()
        .expect("run nut config diff");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("missing")
            || stdout.contains("contents need root")
            || stdout.contains("differs")
    );
}
