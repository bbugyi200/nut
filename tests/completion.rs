use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const NUT: &str = env!("CARGO_BIN_EXE_nut");

fn nut(home: &Path, args: &[&str]) -> Output {
    Command::new(NUT)
        .args(args)
        .env("HOME", home)
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("SHELL", "/bin/zsh")
        .env_remove("ZDOTDIR")
        .env_remove("BASH_ENV")
        .env_remove("BASH_COMPLETION_USER_DIR")
        .output()
        .unwrap()
}

fn success(output: Output) -> String {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn complete(words: &[&str]) -> String {
    let home = tempfile::tempdir().unwrap();
    let mut args = vec!["__complete", "zsh", "--protocol", "1", "--"];
    args.extend(words);
    success(nut(home.path(), &args))
}

#[test]
fn protocol_tree_groups_flags_values_and_paths() {
    let root = complete(&["nut", ""]);
    for (group, commands) in nut_cli::cli::COMMAND_GROUPS {
        for command in *commands {
            assert!(
                root.lines()
                    .any(|line| line.starts_with(&format!("{command}\t"))
                        && line.contains(&format!("\t{group}\tspace"))),
                "{command} missing from {root}"
            );
        }
    }
    assert!(!root.contains("__complete"));
    assert!(complete(&["nut", "status", "--"]).contains("--ups\t"));
    assert!(complete(&["nut", "status", "-"]).contains("-c\t"));
    assert!(complete(&["nut", "--color", "a"]).contains("always\t"));
    let attached = complete(&["nut", "--color=a"]);
    assert!(attached.starts_with("!prefix 8\n"), "{attached}");
    assert!(attached.contains("always\t"));
    assert_eq!(complete(&["nut", "--config", ""]), "!files\n");
    assert_eq!(
        complete(&["nut", "completion", "install", "--target=di"]),
        "!prefix 9\n!dirs\n"
    );
    assert!(complete(&["nut", "bios", "profile", "show", ""]).contains("asus-prime-trx40-pro\t"));
    assert!(complete(&["nut", "drill", ""]).contains("outage\t"));
    assert!(complete(&["nut", "completion", "install", ""]).contains("bash\t"));
    let doctor = complete(&["nut", "doctor", ""]);
    assert!(
        doctor.contains("protect.poweroff-wait\t") && doctor.contains("hw.usb\t"),
        "{doctor}"
    );
    let init = complete(&["nut", "init", "--"]);
    assert!(init.contains("--dry-run\t"), "{init}");
    assert!(init.contains("--expect-plan\t"), "{init}");
    assert!(init.contains("--rotate-secrets\t"), "{init}");
    assert!(init.contains("--yes\t"), "{init}");
    assert!(init.contains("--ups\t"), "{init}");
    assert!(complete(&["nut", "init", "-"]).contains("-n\t"));
    let rollback = complete(&["nut", "rollback", "--"]);
    assert!(rollback.contains("--list\t"), "{rollback}");
    assert!(rollback.contains("--dry-run\t"), "{rollback}");
    assert!(rollback.contains("--yes\t"), "{rollback}");
    assert!(complete(&["nut", "rollback", "-"]).contains("-l\t"));
}

#[test]
fn malformed_skew_and_read_only_failures() {
    let home = tempfile::tempdir().unwrap();
    for args in [
        vec!["__complete"],
        vec!["__complete", "fish", "--protocol", "1", "--", "nut"],
        vec![
            "__complete",
            "zsh",
            "--protocol",
            "1",
            "--protocol",
            "1",
            "--",
            "nut",
        ],
    ] {
        let output = nut(home.path(), &args);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
    }
    for (version, hint) in [("0", "nut completion install"), ("2", "reinstall nut")] {
        let output = success(nut(
            home.path(),
            &["__complete", "bash", "--protocol", version, "--", "nut", ""],
        ));
        assert!(output.starts_with("!message ") && output.contains(hint));
    }
    let output = nut(
        home.path(),
        &[
            "__complete",
            "bash",
            "--protocol=1",
            "--suffix=fix",
            "--",
            "nut",
            "status",
            "--upsd-address",
            "192.0.2.1",
            "--ups",
            "",
        ],
    );
    assert!(output.status.success() && output.stdout.is_empty() && output.stderr.is_empty());
    let output = nut(
        home.path(),
        &[
            "__complete",
            "zsh",
            "--protocol=1",
            "--",
            "nut",
            "status",
            "--upsd-address=192.0.2.1",
            "--ups=",
        ],
    );
    assert!(output.status.success() && output.stdout.is_empty() && output.stderr.is_empty());
    assert_eq!(fs::read_dir(home.path()).unwrap().count(), 0);
}

#[test]
fn live_ups_provider_uses_only_list_ups_and_handles_quoted_description() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut request = String::new();
        BufReader::new(socket.try_clone().unwrap())
            .read_line(&mut request)
            .unwrap();
        assert_eq!(request, "LIST UPS\n");
        socket.write_all(b"BEGIN LIST UPS\nUPS lab \"Lab \\\"UPS\\\"\"\nUPS spare \"Standby\"\nEND LIST UPS\n").unwrap();
    });
    let output = complete(&["nut", "battery", "--upsd-port", &port, "--ups="]);
    server.join().unwrap();
    assert!(output.starts_with("!prefix 6\n"));
    assert!(output.contains("lab\tLab \"UPS\"\tups\tspace"), "{output}");
    assert!(output.contains("spare\tStandby"));
}

#[test]
fn live_ups_provider_bounds_dripping_responses() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        for _ in 0..20 {
            if socket.write_all(b"B").is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    });
    let started = Instant::now();
    assert!(nut_cli::upsd::list_ups("127.0.0.1", port, Duration::from_millis(150)).is_err());
    assert!(
        started.elapsed() < Duration::from_millis(350),
        "a drip must not renew the budget"
    );
    server.join().unwrap();
}

#[test]
fn lifecycle_real_shells_idempotence_dry_run_and_foreign_edits() {
    let home = tempfile::tempdir().unwrap();
    let target = home.path().join("adapters with spaces ' and $chars");
    for shell in ["zsh", "bash"] {
        let args = [
            "completion",
            "install",
            shell,
            "--target",
            target.to_str().unwrap(),
        ];
        let mut preview = args.to_vec();
        preview.push("--dry-run");
        success(nut(home.path(), &preview));
        assert!(!target.exists());
        assert!(!home.path().join("state").exists());
        let output = success(nut(home.path(), &args));
        assert!(output.contains("registration verified"), "{output}");
        let adapter = target.join(if shell == "zsh" { "_nut" } else { "nut" });
        let modified = fs::metadata(&adapter).unwrap().modified().unwrap();
        success(nut(home.path(), &args));
        assert_eq!(
            modified,
            fs::metadata(&adapter).unwrap().modified().unwrap()
        );
        let status: serde_json::Value =
            serde_json::from_str(&success(nut(home.path(), &["completion", "status", "-j"])))
                .unwrap();
        assert_eq!(status["schema_version"], 1);
        let row = status["adapters"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["shell"] == shell)
            .unwrap();
        assert_eq!(row["state"], "current");
        assert_eq!(row["registered"], true);
        fs::write(&adapter, "# foreign edit\n").unwrap();
        assert_eq!(nut(home.path(), &args).status.code(), Some(1));
        assert_eq!(
            nut(home.path(), &["completion", "uninstall", shell])
                .status
                .code(),
            Some(1)
        );
        assert_eq!(fs::read_to_string(&adapter).unwrap(), "# foreign edit\n");
        let mut forced = args.to_vec();
        forced.push("--force");
        success(nut(home.path(), &forced));
        success(nut(
            home.path(),
            &["completion", "uninstall", shell, "--dry-run"],
        ));
        assert!(adapter.exists());
        success(nut(home.path(), &["completion", "uninstall", shell]));
        assert!(!adapter.exists());
        fs::remove_dir(&target).unwrap();
        fs::remove_dir_all(home.path().join("state")).unwrap();
    }
}

#[test]
fn lifecycle_default_targets_and_untracked_file_refusal() {
    let home = tempfile::tempdir().unwrap();
    let fpath = home.path().join("custom-fpath");
    fs::create_dir(&fpath).unwrap();
    fs::write(
        home.path().join(".zshrc"),
        "fpath=(\"$HOME/custom-fpath\" $fpath)\n",
    )
    .unwrap();
    success(nut(home.path(), &["completion", "install", "zsh"]));
    assert!(fpath.join("_nut").exists());
    success(nut(home.path(), &["completion", "install", "bash"]));
    assert!(
        home.path()
            .join("data/bash-completion/completions/nut")
            .exists()
    );
    success(nut(home.path(), &["completion", "uninstall"]));
    fs::write(fpath.join("_nut"), "# user completion").unwrap();
    assert_eq!(
        nut(home.path(), &["completion", "install", "zsh"])
            .status
            .code(),
        Some(1)
    );
}

#[test]
fn bash_adapter_registration_and_actual_candidates() {
    let home = tempfile::tempdir().unwrap();
    let adapter = home.path().join("nut.bash");
    fs::write(&adapter, success(nut(home.path(), &["completion", "bash"]))).unwrap();
    let output = Command::new("bash").args(["--noprofile", "--norc", "-c", r#"
        source "$1"
        complete -p nut
        COMP_LINE='nut sta'; COMP_POINT=${#COMP_LINE}; COMP_WORDS=(nut sta); COMP_CWORD=1
        _nut; printf 'root:%s\n' "${COMPREPLY[@]}"
        COMP_LINE='nut --color=a'; COMP_POINT=${#COMP_LINE}; COMP_WORDS=(nut --color = a); COMP_CWORD=3
        _nut; printf 'color:%s\n' "${COMPREPLY[@]}"
        COMP_LINE="nut --config '' sta"; COMP_POINT=${#COMP_LINE}; COMP_WORDS=(nut --config '' sta); COMP_CWORD=3
        _nut; printf 'empty:%s\n' "${COMPREPLY[@]}"
    "#, "test"]).arg(&adapter).env("HOME", home.path()).env("PATH", format!("{}:/usr/bin:/bin", Path::new(NUT).parent().unwrap().display())).env_remove("BASH_ENV").output().unwrap();
    let output = success(output);
    assert!(output.contains("complete -F _nut nut"));
    assert!(output.contains("root:status"), "{output}");
    assert!(output.contains("color:always"), "{output}");
    assert!(output.contains("empty:status"), "{output}");
}

#[test]
fn zsh_compinit_and_grouped_described_candidates() {
    let home = tempfile::tempdir().unwrap();
    fs::write(
        home.path().join("_nut"),
        success(nut(home.path(), &["completion", "zsh"])),
    )
    .unwrap();
    let output = Command::new("zsh")
        .args([
            "-fc",
            r#"
        fpath=("$1" $fpath); autoload -Uz compinit; compinit -D -i
        [[ ${_comps[nut]} == _nut ]] || exit 1
        _describe() { local name=$5; print -r -- "group:$4"; print -rl -- "${(@P)name}"; }
        words=(nut ''); CURRENT=2; PREFIX=''; SUFFIX=''; curcontext=':complete:nut:'
        _nut
    "#,
            "test",
        ])
        .arg(home.path())
        .env("HOME", home.path())
        .env(
            "PATH",
            format!(
                "{}:/usr/bin:/bin",
                Path::new(NUT).parent().unwrap().display()
            ),
        )
        .output()
        .unwrap();
    let output = success(output);
    assert!(
        output.contains("group:Monitor") && output.contains("status:Show UPS status"),
        "{output}"
    );
    assert!(
        output.contains("group:Set up") && output.contains("group:Prove"),
        "{output}"
    );
}

#[test]
fn install_smoke_all_visible_help() {
    use clap::CommandFactory;
    fn walk(command: &clap::Command, args: &[&str]) {
        let mut invocation = args.to_vec();
        invocation.push("--help");
        success(
            Command::new(std::env::var_os("NUT_SMOKE_BINARY").unwrap_or_else(|| NUT.into()))
                .args(&invocation)
                .output()
                .unwrap(),
        );
        for child in command
            .get_subcommands()
            .filter(|child| !child.is_hide_set())
        {
            let mut args = args.to_vec();
            args.push(child.get_name());
            walk(child, &args);
        }
    }
    walk(&nut_cli::cli::Cli::command(), &[]);
}
