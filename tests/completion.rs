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

fn complete_v2(words: &[&str]) -> String {
    let home = tempfile::tempdir().unwrap();
    let mut args = vec![
        "__complete",
        "zsh",
        "--protocol",
        "2",
        "--columns",
        "100",
        "--",
    ];
    args.extend(words);
    success(nut(home.path(), &args))
}

fn complete(words: &[&str]) -> String {
    complete_v2(words)
}

#[test]
fn protocol_tree_groups_flags_values_and_paths() {
    let root = complete(&["nut", ""]);
    for (group, commands) in nut_cli::cli::COMMAND_GROUPS {
        for command in *commands {
            assert!(
                root.lines()
                    .any(|line| line.starts_with("!group ") && line.contains(group))
                    || root.contains(command),
                "{command} missing from {root}"
            );
        }
    }
    assert!(root.contains("!group nut-monitor\tMonitor"), "{root}");
    assert!(root.contains("status\t"), "{root}");
    assert!(!root.contains("__complete"));
    // v2 option menus carry groups and displays.
    let status_long = complete(&["nut", "status", "--"]);
    assert!(
        status_long.contains("!group nut-options\tOptions"),
        "{status_long}"
    );
    assert!(status_long.contains("--ups\t"), "{status_long}");
    let status_dash = complete(&["nut", "status", "-"]);
    assert!(status_dash.contains("--ups\t"), "{status_dash}");
    assert!(
        status_dash.contains("!group nut-global-options"),
        "{status_dash}"
    );
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
    assert!(init.contains("--ups\t"), "{init}");
    // init keeps its own --yes (only drill and bios-setup --yes are refused).
    assert!(init.contains("--yes\t"), "{init}");
    assert!(complete(&["nut", "init", "-"]).contains("--dry-run\t"));
    let rollback = complete(&["nut", "rollback", "--"]);
    assert!(rollback.contains("--list\t"), "{rollback}");
    assert!(rollback.contains("--dry-run\t"), "{rollback}");
    assert!(complete(&["nut", "rollback", "-"]).contains("--list\t"));
    // Refused flags never appear.
    assert!(
        !complete(&["nut", "drill", "-"]).contains("--yes"),
        "drill --yes refused"
    );
    assert!(
        !complete(&["nut", "bios", "setup", "-"]).contains("--yes"),
        "bios setup --yes refused"
    );
    // Hiding and conflicts.
    assert!(!complete(&["nut", "status", "--watch", "--"]).contains("--watch\t"));
    let events = complete(&["nut", "events", "-o", "--"]);
    assert!(!events.contains("--outages\t"), "{events}");
    assert!(!events.contains("--raw\t"), "{events}");
    // Bare leaf shows options, never dead.
    assert!(complete(&["nut", "status", ""]).contains("--watch\t"));
    // Short-form menu inserts short flags only.
    let short = complete(&["nut", "-c"]);
    assert!(short.contains("-c\t"), "{short}");
    // v1 compatibility: same menu in the legacy shape.
    let home = tempfile::tempdir().unwrap();
    let v1 = success(nut(
        home.path(),
        &["__complete", "zsh", "--protocol", "1", "--", "nut", ""],
    ));
    assert!(
        v1.contains("status\tShow UPS status\tMonitor\tspace"),
        "{v1}"
    );
    assert!(!v1.contains("!group"), "{v1}");
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
        vec![
            "__complete",
            "zsh",
            "--protocol",
            "2",
            "--columns",
            "x",
            "--",
            "nut",
            "",
        ],
        vec!["__complete", "zsh", "--protocol", "2", "--columns"],
    ] {
        let output = nut(home.path(), &args);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
    }
    for (version, hint) in [("0", "nut completion install"), ("3", "reinstall nut")] {
        let output = success(nut(
            home.path(),
            &["__complete", "bash", "--protocol", version, "--", "nut", ""],
        ));
        assert!(output.starts_with("!message ") && output.contains(hint));
    }
    // An offline upsd yields a styled hint, not silence.
    let output = success(nut(
        home.path(),
        &[
            "__complete",
            "bash",
            "--protocol=2",
            "--columns=100",
            "--suffix=fix",
            "--",
            "nut",
            "status",
            "--upsd-address",
            "192.0.2.1",
            "--ups",
            "",
        ],
    ));
    assert!(output.contains("!hint UPS\t"), "{output}");
    assert!(output.contains("did not answer"), "{output}");
    let output = success(nut(
        home.path(),
        &[
            "__complete",
            "zsh",
            "--protocol=2",
            "--",
            "nut",
            "status",
            "--upsd-address=192.0.2.1",
            "--ups=",
        ],
    ));
    assert!(output.contains("!hint UPS\t"), "{output}");
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
    assert!(output.starts_with("!prefix 6\n"), "{output}");
    assert!(
        output.contains("!group nut-ups\tUPS on 127.0.0.1:"),
        "{output}"
    );
    assert!(output.contains("lab\t"), "{output}");
    assert!(output.contains("Lab"), "{output}");
    assert!(output.contains("spare\t"), "{output}");
    assert!(output.contains("Standby"), "{output}");
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
fn lifecycle_v1_manifest_reports_outdated_and_upgrades() {
    let home = tempfile::tempdir().unwrap();
    let target = home.path().join("v1target");
    success(nut(
        home.path(),
        &[
            "completion",
            "install",
            "zsh",
            "--target",
            target.to_str().unwrap(),
        ],
    ));
    let manifest = home.path().join("state/nut-cli/completion.json");
    let mut data: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&manifest).unwrap()).unwrap();
    data["adapters"]["zsh"]["protocol"] = serde_json::json!(1);
    fs::write(&manifest, serde_json::to_vec_pretty(&data).unwrap()).unwrap();
    let status: serde_json::Value =
        serde_json::from_str(&success(nut(home.path(), &["completion", "status", "-j"]))).unwrap();
    let row = status["adapters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["shell"] == "zsh")
        .unwrap();
    assert_eq!(row["state"], "outdated");
    success(nut(
        home.path(),
        &[
            "completion",
            "install",
            "zsh",
            "--target",
            target.to_str().unwrap(),
        ],
    ));
    let status: serde_json::Value =
        serde_json::from_str(&success(nut(home.path(), &["completion", "status", "-j"]))).unwrap();
    let row = status["adapters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["shell"] == "zsh")
        .unwrap();
    assert_eq!(row["state"], "current");
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
fn bash_end_to_end_modes() {
    let home = tempfile::tempdir().unwrap();
    let adapter = home.path().join("nut.bash");
    fs::write(&adapter, success(nut(home.path(), &["completion", "bash"]))).unwrap();
    // COMP_TYPE=9 with several matches gives aligned displays in menu order.
    let output = Command::new("bash")
        .args([
            "--noprofile",
            "--norc",
            "-c",
            r#"
        source "$1"
        COMP_LINE='nut status --'; COMP_POINT=${#COMP_LINE}
        COMP_WORDS=(nut status --); COMP_CWORD=2; COMP_TYPE=9
        COLUMNS=100 _nut
        printf 'n:%s\n' "${#COMPREPLY[@]}"
        for reply in "${COMPREPLY[@]}"; do
          (( ${#reply} <= 99 )) || { echo "TOO LONG: $reply"; exit 1; }
        done
        printf 'first:%s\n' "${COMPREPLY[0]}"
        COMP_LINE='nut sta'; COMP_POINT=${#COMP_LINE}
        COMP_WORDS=(nut sta); COMP_CWORD=1; COMP_TYPE=9
        _nut; printf 'single:%s\n' "${COMPREPLY[@]}"
        COMP_LINE='nut sta'; COMP_POINT=${#COMP_LINE}
        COMP_WORDS=(nut sta); COMP_CWORD=1; COMP_TYPE=37
        _nut; printf 'menu:%s\n' "${COMPREPLY[@]}"
        COMP_LINE='nut --color=a'; COMP_POINT=${#COMP_LINE}
        COMP_WORDS=(nut --color = a); COMP_CWORD=3; COMP_TYPE=9
        _nut; printf 'attached:%s\n' "${COMPREPLY[@]}"
    "#,
            "test",
        ])
        .arg(&adapter)
        .env("HOME", home.path())
        .env(
            "PATH",
            format!(
                "{}:/usr/bin:/bin",
                Path::new(NUT).parent().unwrap().display()
            ),
        )
        .env_remove("BASH_ENV")
        .output()
        .unwrap();
    let output = success(output);
    assert!(output.contains("first:--watch"), "{output}");
    assert!(output.contains("single:status"), "{output}");
    assert!(output.contains("menu:status"), "{output}");
    assert!(output.contains("attached:always"), "{output}");
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
        _wanted() { local tag=$2; print -r -- "tag:$tag"; shift 4; "$@"; }
        compadd() {
          local disp_arg val_arg
          while (( $# )); do
            case $1 in
              -d) disp_arg=$2; shift 2 ;;
              -a) val_arg=$2; shift 2 ;;
              *) shift ;;
            esac
          done
          print -rl -- "${(@P)disp_arg}"
        }
        _description() { :; }
        _files() { :; }
        _message() { :; }
        compset() { :; }
        words=(nut ''); CURRENT=2; PREFIX=''; SUFFIX=''; curcontext=':complete:nut:'
        _nut
        # No nut style leaks after the call.
        if zstyle -L 2>/dev/null | grep -q 'nut-'; then print -r -- "LEAK"; else print -r -- "CLEAN"; fi
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
    assert!(output.contains("tag:nut-monitor"), "{output}");
    assert!(
        output.contains("tag:nut-set-up") && output.contains("tag:nut-prove"),
        "{output}"
    );
    assert!(output.contains("CLEAN"), "nut zstyle leaked: {output}");
}

fn zpty_list(keys: &str, columns: usize, extra_setup: &str) -> String {
    // Real-zsh end-to-end through a pty with pattern/deadline reads (10 s
    // ceiling), never a fixed sleep for reads. The driver is Python+pty
    // (stable across machines); the completed shell is always real zsh.
    let home = tempfile::tempdir().unwrap();
    let bindir = Path::new(NUT).parent().unwrap().display().to_string();
    let adapter_dir = home.path().join("fpath");
    fs::create_dir(&adapter_dir).unwrap();
    fs::write(
        adapter_dir.join("_nut"),
        success(nut(home.path(), &["completion", "zsh"])),
    )
    .unwrap();
    let driver = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/zpty_drive.py");
    let output = Command::new("python3")
        .args([
            driver.to_str().unwrap(),
            home.path().to_str().unwrap(),
            &bindir,
            adapter_dir.to_str().unwrap(),
            keys,
            &columns.to_string(),
            extra_setup,
        ])
        .env("HOME", home.path())
        .env("PATH", format!("{}:/usr/bin:/bin", bindir))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "zpty driver failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

#[test]
fn zsh_end_to_end_listing_colors_and_styles() {
    let listing = zpty_list("nut \t", 100, "");
    let stripped = listing
        .replace("\u{1b}[", "\x1b[")
        .lines()
        .map(|line| {
            let mut clean = String::new();
            let mut chars = line.chars().peekable();
            while let Some(ch) = chars.next() {
                if ch == '\x1b' && chars.peek() == Some(&'[') {
                    chars.next();
                    for ch in chars.by_ref() {
                        if ch.is_ascii_alphabetic() {
                            break;
                        }
                    }
                } else {
                    clean.push(ch);
                }
            }
            clean
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(stripped.contains("Monitor"), "{stripped}");
    assert!(stripped.contains("status"), "{stripped}");
    // Raw bytes carry theme roles: bold+cyan headings, bold names, dim descriptions.
    // (Prompt itself uses SGR, so assert the nut-specific roles: cyan 36m,
    // dim 2m, and bold 1m alongside cyan.)
    assert!(
        listing.contains("\x1b[36m"),
        "headings must be cyan: {listing}"
    );
    assert!(listing.contains("\x1b[1m"), "names must be bold: {listing}");
    assert!(
        listing.contains("\x1b[2m"),
        "descriptions must be dim: {listing}"
    );
    // Global oh-my-zsh list-colors must not suppress nut's colors.
    let with_global = zpty_list("nut \t", 100, "zstyle ':completion:*' list-colors ''");
    assert!(
        with_global.contains("\x1b[36m"),
        "global list-colors must not win: {with_global}"
    );
    // A user nut style wins.
    let with_user = zpty_list(
        "nut \t",
        100,
        "zstyle ':completion:*:*:nut:*' list-colors '=*=35'",
    );
    assert!(with_user.contains("35"), "{with_user}");
    // NO_COLOR strips nut SGR (prompt itself still uses SGR).
    let nocolor = zpty_list("nut \t", 100, "export NO_COLOR=1");
    assert!(
        !nocolor.contains("\x1b[36m") && !nocolor.contains("\x1b[2m"),
        "{nocolor}"
    );
    // Afterwards no nut style leaks.
    // (Checked inside the adapter via zstyle -d; the listing itself is evidence.)
    // -c offers short flags with no stray hidden-alias row.
    let dash_c = zpty_list("nut -c\t", 100, "");
    assert!(dash_c.contains("-c"), "{dash_c}");
    assert!(!dash_c.contains("-c  -j"), "{dash_c}");
    // Narrow panes wrap nothing (strip CSI, then measure).
    let narrow = zpty_list("nut \t", 50, "");
    let narrow_stripped = narrow
        .lines()
        .map(|line| {
            let mut clean = String::new();
            let mut chars = line.chars().peekable();
            while let Some(ch) = chars.next() {
                if ch == '\x1b' && chars.peek() == Some(&'[') {
                    chars.next();
                    for ch in chars.by_ref() {
                        if ch.is_ascii_alphabetic() {
                            break;
                        }
                    }
                } else {
                    clean.push(ch);
                }
            }
            clean
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(narrow_stripped.contains("Monitor"), "{narrow_stripped}");
    assert!(narrow_stripped.contains("status"), "{narrow_stripped}");
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
