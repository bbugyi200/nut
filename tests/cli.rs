use std::process::Command;

#[test]
fn help_describes_the_bootstrap_without_touching_hardware() {
    let output = Command::new(env!("CARGO_BIN_EXE_nut"))
        .arg("--help")
        .output()
        .expect("run nut help");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 help");
    assert!(stdout.contains("Manage, check, and prove NUT UPS protection"));
    assert!(stdout.contains("Bootstrap only"));
    assert!(output.stderr.is_empty());
}

#[test]
fn version_matches_the_package() {
    let output = Command::new(env!("CARGO_BIN_EXE_nut"))
        .arg("--version")
        .output()
        .expect("run nut version");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).expect("UTF-8 version"),
        format!("nut {}\n", env!("CARGO_PKG_VERSION"))
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn unknown_options_fail_as_usage_errors() {
    let output = Command::new(env!("CARGO_BIN_EXE_nut"))
        .arg("--not-a-nut-option")
        .output()
        .expect("run nut with invalid option");
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());
}
