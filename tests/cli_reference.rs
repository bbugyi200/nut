use std::process::{Command, Output};

fn nut(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_nut"))
        .args(args)
        .output()
        .expect("run nut")
}

#[test]
fn root_help_matches_reference() {
    let output = nut(&["--help"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        include_str!("reference/nut-help.txt")
    );
}

#[test]
fn config_help_matches_reference() {
    let output = nut(&["config", "--help"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        include_str!("reference/config-help.txt")
    );
}

#[test]
fn bios_help_matches_reference() {
    let output = nut(&["bios", "--help"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        include_str!("reference/bios-help.txt")
    );
}

#[test]
fn invalid_command_uses_usage_exit_code() {
    let output = nut(&["not-a-command"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("unrecognized subcommand")
    );
}

#[test]
fn config_show_matches_reference_example() {
    let output = nut(&["--config", "examples/host.toml", "config", "show"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        include_str!("reference/config-show.txt")
    );
}

#[test]
fn config_show_json_has_schema_version_and_no_secret_fields() {
    let output = nut(&["--json", "--config", "examples/host.toml", "config", "show"]);
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["host"]["name"], "example-host");
    assert!(output.stdout.windows(8).all(|window| window != b"password"));
}

#[test]
fn events_help_matches_reference() {
    let output = nut(&["events", "--help"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        include_str!("reference/events-help.txt")
    );
}

#[test]
fn doctor_help_matches_reference() {
    let output = nut(&["doctor", "--help"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        include_str!("reference/doctor-help.txt")
    );
}

#[test]
fn battery_help_matches_reference() {
    let output = nut(&["battery", "--help"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        include_str!("reference/battery-help.txt")
    );
}

#[test]
fn beeper_help_matches_reference() {
    let output = nut(&["beeper", "--help"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        include_str!("reference/beeper-help.txt")
    );
}

#[test]
fn notify_help_matches_reference() {
    let output = nut(&["notify", "--help"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        include_str!("reference/notify-help.txt")
    );
}

#[test]
fn init_help_matches_reference() {
    let output = nut(&["init", "--help"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        include_str!("reference/init-help.txt")
    );
}

#[test]
fn rollback_help_matches_reference() {
    let output = nut(&["rollback", "--help"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        include_str!("reference/rollback-help.txt")
    );
}
