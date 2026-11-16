use std::process::Command;

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
