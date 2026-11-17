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
        let group = group.to_lowercase();
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
    assert!(
        root.lines().all(|line| line.starts_with('!')
            || !line.split('\t').next().unwrap_or_default().starts_with('-')),
        "root empty word offered options: {root}"
    );
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
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && output.stderr.is_empty(),
        "{stdout}"
    );
    assert!(
        stdout.contains("!message upsd at 192.0.2.1:3493 did not answer"),
        "{stdout}"
    );
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
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && output.stderr.is_empty(),
        "{stdout}"
    );
    assert!(
        stdout.starts_with("!prefix 6\n!message upsd at 192.0.2.1:3493 did not answer"),
        "{stdout}"
    );
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
    assert!(
        output.contains("lab\tLab \"UPS\"\tUPS devices\tspace"),
        "{output}"
    );
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
        output.contains("group:monitor") && output.contains("status:Show UPS status"),
        "{output}"
    );
    assert!(
        output.contains("group:set up") && output.contains("group:prove"),
        "{output}"
    );

    let paired = Command::new("zsh")
        .args([
            "-fc",
            r#"
        fpath=("$1" $fpath); autoload -Uz compinit; compinit -D -i
        _describe() { local name=$5; print -r -- "group:$4"; print -rl -- "${(@P)name}"; }
        words=(nut status '-'); CURRENT=3; PREFIX='-'; SUFFIX=''; curcontext=':complete:nut:'
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
    let paired = success(paired);
    let options = paired
        .split("group:")
        .find(|chunk| chunk.starts_with("options"));
    let options = options.unwrap_or(&paired);
    assert!(
        options.contains("-h:Print help") && options.contains("--help:Print help"),
        "{paired}"
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

const ZSH_GOLDEN: &str = "tests/reference/zsh-completion.txt";
const GOLDENS_ENV: &str = "NUT_UPDATE_GOLDENS";

fn have_zsh() -> bool {
    Command::new("zsh")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn strip_ansi_and_cr(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    for next in chars.by_ref() {
                        if next.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    for next in chars.by_ref() {
                        if next == '\u{7}' || next == '\u{1b}' {
                            break;
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        if ch == '\u{8}' {
            out.pop();
            continue;
        }
        if ch != '\r' && ch != '\u{7}' {
            out.push(ch);
        }
    }
    out
}

fn trim_command_echo(line: &str) -> String {
    let line = line.trim_end();
    let Some(split) = line.find(" -- ") else {
        return line.to_owned();
    };
    let (left, right) = line.split_at(split + 4);
    if let Some(pos) = right.find("nut") {
        let description = right[..pos].trim_end();
        if !description.is_empty() {
            return format!("{left}{description}");
        }
    }
    line.to_owned()
}

fn extract_menu(raw: &str) -> String {
    let stripped = strip_ansi_and_cr(raw);
    let lines: Vec<&str> = stripped.lines().map(str::trim_end).collect();
    let Some(start) = lines.iter().position(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with("── ") || trimmed.starts_with("== ")
    }) else {
        return stripped;
    };
    let mut end = start;
    for (index, line) in lines.iter().enumerate().skip(start) {
        let trimmed = line.trim_start();
        if trimmed.starts_with("── ")
            || trimmed.starts_with("== ")
            || trimmed.contains(" -- ")
            || trimmed.is_empty()
        {
            end = index;
            continue;
        }
        break;
    }
    while end > start && lines[end].is_empty() {
        end -= 1;
    }
    let body: Vec<String> = lines[start..=end]
        .iter()
        .map(|line| trim_command_echo(line))
        .collect();
    let mut text = body.join("\n");
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

fn output_with_deadline(mut command: Command, deadline: Duration) -> std::io::Result<Output> {
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    let mut child = command.spawn()?;
    let started = Instant::now();
    loop {
        match child.try_wait()? {
            Some(_) => return child.wait_with_output(),
            None if started.elapsed() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "zpty deadline exceeded",
                ));
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn run_zpty(home: &Path, extra_env: &[(&str, &str)]) -> Option<(String, String)> {
    if !have_zsh() {
        println!("skipped: zsh not found on PATH");
        return None;
    }
    fs::write(
        home.join("_nut"),
        success(nut(home, &["completion", "zsh"])),
    )
    .unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(NUT, home.join("nut")).unwrap();
    fs::write(home.join("driver.zsh"), ZPTY_DRIVER).unwrap();
    let mut command = Command::new("zsh");
    command.arg("-f").arg(home.join("driver.zsh")).arg(home);
    command.env("HOME", home);
    command.env("TERM", "xterm-256color");
    command.env("PATH", format!("{}:/usr/bin:/bin", home.display()));
    command.env_remove("ZDOTDIR");
    command.env_remove("NO_COLOR");
    command.env_remove("CLICOLOR_FORCE");
    for (key, value) in extra_env {
        command.env(key, value);
    }
    let output = match output_with_deadline(command, Duration::from_secs(30)) {
        Ok(output) => output,
        Err(error) => panic!("zpty readiness deadline exceeded: {error}"),
    };
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if stdout.contains("ZPTY-MISSING") {
        println!("skipped: zsh/zpty not usable on this host");
        return None;
    }
    assert!(
        output.status.success(),
        "zpty driver failed, stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    Some((stdout, stderr))
}

fn section<'a>(stdout: &'a str, name: &str) -> &'a str {
    let start = format!("BEGIN {name}\n");
    let end = format!("\nEND {name}");
    let from = stdout
        .find(&start)
        .unwrap_or_else(|| panic!("missing {start:?} in {stdout}"));
    let rest = &stdout[from + start.len()..];
    let to = rest
        .find(&end)
        .unwrap_or_else(|| panic!("missing {end:?} in {stdout}"));
    &rest[..to]
}

const ZPTY_DRIVER: &str = r#"
zmodload zsh/zpty || { print "ZPTY-MISSING"; exit 2 }
zmodload zsh/zselect || { print "ZSELECT-MISSING"; exit 2 }
TMP=$1
typeset -i deadline=$((SECONDS + 20))
typeset -i sync_count=0
READ_RESULT=
PTY_FD=
wait_for() {
  local expected=$1 remaining chunk
  READ_RESULT=
  while true; do
    if chunk=$(zpty -rt pb); then
      READ_RESULT+=$chunk
      if [[ $READ_RESULT == *"$expected"* ]]; then
        return 0
      fi
    fi
    remaining=$((deadline - SECONDS))
    (( remaining > 0 )) || { print -r -- "READINESS TIMEOUT: $READ_RESULT"; return 1; }
    zselect -t 10 "$PTY_FD" || true
  done
}
reset_line() {
  (( sync_count += 1 ))
  zpty -w -n pb $'\x15'
  zpty -w pb "print -r -- SY''NC-$sync_count"
  wait_for "SYNC-$sync_count"
}
export PATH="$TMP:$PATH"
zpty -d pb 2>/dev/null
zpty -b pb zsh -f
PTY_FD=$REPLY
zpty -w pb "PS1='ZP> '; bindkey -e; LISTMAX=1000; setopt nolistbeep"
zpty -w pb "stty cols 100 rows 40"
zpty -w pb "fpath=($TMP \$fpath)"
zpty -w pb "autoload -Uz compinit && compinit -D -i && print -r -- COMPINIT-DONE"
wait_for COMPINIT-DONE || { print -r -- "SETUP-READ-FAILED"; zpty -d pb; exit 1 }
print -r -- "SETUP OK"
zpty -w -n pb $'nut \t'
if wait_for monitor; then
  print -r -- "BEGIN root"
  print -r -- "$READ_RESULT"
  print -r -- "END root"
else
  print -r -- "ROOT MISS"
fi
reset_line || { print -r -- "PROMPT-READ-FAILED"; zpty -d pb; exit 1 }
zpty -w -n pb $'nut status -\t'
if wait_for options; then
  print -r -- "BEGIN status"
  print -r -- "$READ_RESULT"
  print -r -- "END status"
else
  print -r -- "STATUS MISS"
fi
reset_line || { print -r -- "PROMPT-READ-FAILED"; zpty -d pb; exit 1 }
zpty -w -n pb $'nut doctor \t'
if wait_for hardware; then
  print -r -- "BEGIN doctor"
  print -r -- "$READ_RESULT"
  print -r -- "END doctor"
else
  print -r -- "DOCTOR MISS"
fi
reset_line || { print -r -- "PROMPT-READ-FAILED"; zpty -d pb; exit 1 }
zpty -w -n pb $'nut --color \t'
if wait_for always; then
  print -r -- "BEGIN color"
  print -r -- "$READ_RESULT"
  print -r -- "END color"
else
  print -r -- "COLOR MISS"
fi
zpty -d pb
"#;

#[test]
fn real_zsh_menus_match_golden() {
    let home = tempfile::tempdir().unwrap();
    let Some((stdout, stderr)) =
        run_zpty(home.path(), &[("LANG", "C.UTF-8"), ("LC_ALL", "C.UTF-8")])
    else {
        return;
    };
    assert!(
        stdout.contains("SETUP OK")
            && stdout.contains("BEGIN root")
            && stdout.contains("BEGIN status")
            && stdout.contains("BEGIN doctor")
            && stdout.contains("BEGIN color"),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let root_raw = section(&stdout, "root");
    assert!(
        root_raw.contains("\u{1b}[") && (root_raw.contains("36") || root_raw.contains("[1m")),
        "headers should be bold cyan, got {root_raw:?}"
    );
    assert!(
        root_raw.contains("\u{1b}[1m") || root_raw.contains("[1;"),
        "headers should be bold, got {root_raw:?}"
    );
    let menus = format!(
        "== nut <Tab> ==\n{}== nut status -<Tab> ==\n{}== nut doctor <Tab> ==\n{}== nut --color <Tab> ==\n{}",
        extract_menu(root_raw),
        extract_menu(section(&stdout, "status")),
        extract_menu(section(&stdout, "doctor")),
        extract_menu(section(&stdout, "color")),
    );
    let golden_path = Path::new(env!("CARGO_MANIFEST_DIR")).join(ZSH_GOLDEN);
    if std::env::var_os(GOLDENS_ENV).is_some() {
        fs::write(&golden_path, &menus).unwrap();
    }
    let expected = fs::read_to_string(&golden_path).unwrap_or_default();
    assert_eq!(
        menus, expected,
        "zsh menus drifted; run NUT_UPDATE_GOLDENS=1 cargo test --test completion -- --exact real_zsh_menus_match_golden"
    );
    let root_menu = extract_menu(root_raw);
    let docs = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/completion.md"))
        .unwrap();
    assert!(
        docs.contains(root_menu.trim_end()),
        "docs/completion.md must contain the golden root menu verbatim"
    );

    let home = tempfile::tempdir().unwrap();
    let Some((no_color, _)) = run_zpty(
        home.path(),
        &[
            ("LANG", "C.UTF-8"),
            ("LC_ALL", "C.UTF-8"),
            ("NO_COLOR", "1"),
        ],
    ) else {
        return;
    };
    let root = section(&no_color, "root");
    assert!(
        !root.contains("36m") && !root.contains(";36"),
        "NO_COLOR=1 headers must not carry cyan: {root:?}"
    );
    assert!(
        root.contains("\u{1b}[1m") || root.contains("[1;"),
        "NO_COLOR=1 headers should remain bold: {root:?}"
    );

    let home = tempfile::tempdir().unwrap();
    let Some((ascii, _)) = run_zpty(
        home.path(),
        &[("LANG", "C"), ("LC_ALL", "C"), ("LC_CTYPE", "C")],
    ) else {
        return;
    };
    let ascii_root = strip_ansi_and_cr(section(&ascii, "root"));
    assert!(
        ascii_root.contains("== monitor =="),
        "LANG=C should use ASCII headers: {ascii_root}"
    );
}
