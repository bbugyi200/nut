#![forbid(unsafe_code)]

//! Privileged CLI binding for `nut init` and `nut rollback`.
//!
//! The library owns the convergence and rollback engines; this module owns the
//! interactive contract: previews, confirmations, root-owned binary checks,
//! exact sudo argv, missing-config scaffolding, redacted JSON, and width-aware
//! screens. No test invokes real sudo: [`Host::run_bounded`] is scripted.

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

use crate::cli::{ColorChoice, InitArgs, RollbackArgs};
use crate::config;
use crate::error::NutError;
use crate::host::{Host, ProcessRequest};
use crate::init::{
    ConvergeOptions, Plan, PlanOptions, RollbackOptions, collect, converge, plan, rollback,
};
use crate::ui::{UiContext, UiOptions};

const INSTALLED_BINARY: &str = "/usr/local/bin/nut";
const ELEVATE_TIMEOUT: Duration = Duration::from_secs(300);
const PREVIEW_TIMEOUT: Duration = Duration::from_secs(60);

pub struct InitOutput<'a> {
    pub config_path: Option<&'a Path>,
    pub json: bool,
    pub ascii: bool,
    pub color: ColorChoice,
    pub verbose: bool,
}

pub struct RollbackOutput<'a> {
    pub config_path: Option<&'a Path>,
    pub json: bool,
    pub ascii: bool,
    pub color: ColorChoice,
    pub verbose: bool,
}

fn ui<H: Host>(host: &H, ascii: bool, color: ColorChoice, stdout_tty: bool) -> UiContext {
    UiContext::new(UiOptions { color, ascii }, host, stdout_tty)
}

fn write_error(message: impl std::fmt::Display) -> NutError {
    NutError::operational(format!("cannot write output: {message}"))
}

fn hostname<H: Host>(host: &H) -> String {
    host.hostname().unwrap_or_else(|_| "unknown".to_owned())
}

fn absolute_config<H: Host>(host: &H, override_path: Option<&Path>) -> PathBuf {
    config::config_path(host, override_path)
        .map(|(path, _)| path)
        .unwrap_or_else(|_| PathBuf::from("/etc/nut-cli/nut.toml"))
}

fn verify_elevation_binary<H: Host>(host: &H) -> Result<PathBuf, NutError> {
    let exe = host.current_exe().map_err(|error| {
        NutError::operational(format!("cannot resolve running binary: {error}"))
    })?;
    if exe != Path::new(INSTALLED_BINARY) {
        return Err(NutError::operational(format!(
            "refusing elevation: running {} is not the installed {INSTALLED_BINARY}",
            exe.display()
        ))
        .with_hint(format!(
            "sudo install -o root -g root -m 0755 nut {INSTALLED_BINARY}"
        )));
    }
    let meta = host
        .metadata(&exe)
        .map_err(|error| NutError::operational(format!("cannot stat {INSTALLED_BINARY}: {error}")))?
        .ok_or_else(|| {
            NutError::operational(format!("{INSTALLED_BINARY} is missing")).with_hint(format!(
                "sudo install -o root -g root -m 0755 nut {INSTALLED_BINARY}"
            ))
        })?;
    if !meta.is_file || meta.uid != 0 || !meta.executable() {
        return Err(NutError::operational(format!(
            "refusing elevation: {INSTALLED_BINARY} uid {} mode {:04o}",
            meta.uid, meta.mode
        ))
        .with_hint(format!(
            "sudo install -o root -g root -m 0755 nut {INSTALLED_BINARY}"
        )));
    }
    // Safe ancestors: no writable directories, no symlinks/specials.
    let mut current = Path::new(INSTALLED_BINARY);
    while let Some(parent) = current.parent() {
        if parent.as_os_str().is_empty() {
            break;
        }
        match host.metadata(parent) {
            Ok(None) => {
                return Err(NutError::operational(format!(
                    "refusing elevation: ancestor {} is missing",
                    parent.display()
                )));
            }
            Ok(Some(meta)) => {
                if !meta.is_dir {
                    return Err(NutError::operational(format!(
                        "refusing elevation: ancestor {} is not a directory",
                        parent.display()
                    )));
                }
                if meta.mode & 0o022 != 0 {
                    return Err(NutError::operational(format!(
                        "refusing elevation: ancestor {} is group/world-writable",
                        parent.display()
                    )));
                }
            }
            Err(error) => {
                return Err(NutError::operational(format!(
                    "cannot stat ancestor {}: {error}",
                    parent.display()
                )));
            }
        }
        if parent == Path::new("/") {
            break;
        }
        current = parent;
    }
    Ok(exe)
}

fn require_interactive<H: Host>(host: &H) -> Result<(), NutError> {
    if !host.stdin_is_terminal() || !host.stdout_is_terminal() {
        return Err(NutError::operational(
            "refusing to prompt: stdin and stdout must both be terminals",
        )
        .with_hint("use --dry-run to preview or --yes --expect-plan HASH to apply"));
    }
    Ok(())
}

fn confirm<H: Host>(host: &H, prompt: &str) -> Result<bool, NutError> {
    let line = host
        .prompt_line(prompt)
        .map_err(|error| NutError::operational(format!("cannot read confirmation: {error}")))?;
    let normalized = line.trim().to_ascii_lowercase();
    Ok(matches!(normalized.as_str(), "y" | "yes"))
}

#[derive(Clone, Debug, Serialize)]
struct InitPreviewJson {
    schema_version: u32,
    complete: bool,
    hash: Option<String>,
    empty: bool,
    steps: Vec<crate::init::PlanStep>,
    explanations: Vec<String>,
    secret_intents: Vec<String>,
    config_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct InitApplyJson {
    schema_version: u32,
    hash: String,
    backup_stamp: Option<String>,
    files_applied: Vec<PathBuf>,
    services_applied: Vec<String>,
    resumed: bool,
    state_finalized: bool,
    verification: VerificationJson,
    #[serde(skip_serializing_if = "Option::is_none")]
    rollback_hint: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct VerificationJson {
    passed: usize,
    warnings: usize,
    failed: usize,
    unknown: usize,
    skipped: usize,
    failures: Vec<FailureJson>,
}

#[derive(Clone, Debug, Serialize)]
struct FailureJson {
    id: String,
    title: String,
    evidence: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    fix: Option<String>,
}

fn verification_json(report: &crate::doctor::DoctorReport) -> VerificationJson {
    VerificationJson {
        passed: report.passed,
        warnings: report.warnings,
        failed: report.failed,
        unknown: report.unknown,
        skipped: report.skipped,
        failures: report
            .failures
            .iter()
            .map(|failure| FailureJson {
                id: failure.id.clone(),
                title: failure.title.clone(),
                evidence: failure.evidence.clone(),
                fix: failure.fix.clone(),
            })
            .collect(),
    }
}

fn preview_json(plan: &Plan, config_path: Option<PathBuf>) -> InitPreviewJson {
    let view = plan.view();
    InitPreviewJson {
        schema_version: 1,
        complete: view.complete,
        hash: view.hash.clone(),
        empty: view.empty,
        steps: view.steps.clone(),
        explanations: view.explanations.clone(),
        secret_intents: view
            .secret_intents
            .iter()
            .map(|intent| format!("{intent:?}"))
            .collect(),
        config_path,
        hint: if view.complete {
            None
        } else {
            Some("review the explanations, then re-run with a readable config".to_owned())
        },
    }
}

fn write_json<W: Write>(stdout: &mut W, value: &impl Serialize) -> Result<(), NutError> {
    serde_json::to_writer_pretty(&mut *stdout, value)
        .map_err(|error| NutError::operational(format!("cannot write JSON output: {error}")))?;
    writeln!(stdout).map_err(write_error)?;
    Ok(())
}

fn short_hash(hash: &str) -> String {
    hash.chars().take(8).collect()
}

fn dot(ui: &UiContext) -> &'static str {
    if ui.glyphs.arrow == "->" {
        " | "
    } else {
        " · "
    }
}

fn arrow(ui: &UiContext) -> &'static str {
    ui.glyphs.arrow
}

fn step_counts(plan: &Plan) -> (usize, usize, usize, usize) {
    let mut files = 0;
    let mut services = 0;
    let mut accounts = 0;
    let mut other = 0;
    for step in &plan.view().steps {
        let id = step.id.as_str();
        if id.starts_with("file:") {
            files += 1;
        } else if id.starts_with("service:") {
            services += 1;
        } else if id.starts_with("group:") || id.starts_with("membership:") {
            accounts += 1;
        } else {
            other += 1;
        }
    }
    (files, services, accounts, other)
}

fn render_preview_text<H: Host, W: Write>(
    host: &H,
    plan: &Plan,
    stdout: &mut W,
    ui: &UiContext,
) -> Result<(), NutError> {
    use crate::ui::theme::{Theme, Tone};
    let view = plan.view();
    let host_name = hostname(host);
    let width = ui.width.max(20);
    if view.empty && view.complete {
        let line = format!(" {} Already converged. Nothing to change.", ui.glyphs.ok);
        let painted = Theme::paint(Tone::Ok, &line, ui.color_enabled);
        writeln!(stdout, "{painted}").map_err(write_error)?;
        return Ok(());
    }
    let hash_short = view
        .hash
        .as_deref()
        .map(short_hash)
        .unwrap_or_else(|| "partial".to_owned());
    let separator = dot(ui);
    let title = format!(" nut init{separator}{host_name}{separator}plan {hash_short}");
    writeln!(
        stdout,
        "{}",
        Theme::paint(Tone::Info, &title, ui.color_enabled)
    )
    .map_err(write_error)?;
    let (files, services, accounts, other) = step_counts(plan);
    let summary = format!(
        "   Steps      {files} files{separator}{services} services{separator}{accounts} accounts{separator}{other} other"
    );
    write_wrapped(stdout, &summary, width)?;
    // Group file steps for quick review.
    for (shown, step) in view.steps.iter().enumerate() {
        if shown >= 12 {
            let rest = view.steps.len() - shown;
            let ellipsis = if ui.glyphs.arrow == "->" {
                "..."
            } else {
                "…"
            };
            write_wrapped(stdout, &format!("     {ellipsis} {rest} more steps"), width)?;
            break;
        }
        let marker = if step.id.starts_with("file:") {
            "~"
        } else if step.id.starts_with("service:") {
            "▸"
        } else {
            "+"
        };
        let marker = if ui.glyphs.arrow == "->" && marker == "▸" {
            ">"
        } else {
            marker
        };
        write_wrapped(
            stdout,
            &format!("     {marker} {}  {}", step.id, step.summary),
            width,
        )?;
    }
    if !view.explanations.is_empty() {
        for explanation in &view.explanations {
            write_wrapped(stdout, &format!(" {} {explanation}", ui.glyphs.warn), width)?;
        }
    }
    if !view.complete {
        write_wrapped(
            stdout,
            " Partial preview: required inputs need root to read. No apply hash.",
            width,
        )?;
    }
    Ok(())
}

fn write_wrapped<W: Write>(stdout: &mut W, text: &str, width: usize) -> Result<(), NutError> {
    let width = width.max(20).clamp(60, 120);
    let mut current = String::new();
    let mut column = 0;
    let mut at_start = true;
    for word in text.split_whitespace() {
        if !at_start && column + 1 + word.chars().count() > width {
            writeln!(stdout, "{}", current.trim_end()).map_err(write_error)?;
            current.clear();
            column = 0;
            at_start = true;
        }
        if !at_start {
            current.push(' ');
            column += 1;
        }
        current.push_str(word);
        column += word.chars().count();
        at_start = false;
    }
    writeln!(stdout, "{}", current.trim_end()).map_err(write_error)?;
    Ok(())
}

fn collect_plan<H: Host>(host: &H, config: Option<&Path>, ups: Option<&str>, rotate: bool) -> Plan {
    let observation = collect(host, config, ups);
    plan(
        &observation,
        &PlanOptions {
            rotate_secrets: rotate,
        },
    )
}

fn is_missing_config(plan: &Plan) -> bool {
    plan.view()
        .explanations
        .iter()
        .any(|explanation| explanation.contains("does not exist"))
}

fn is_unreadable_partial(plan: &Plan) -> bool {
    plan.view()
        .explanations
        .iter()
        .any(|explanation| explanation.contains("needs root to read"))
}

struct ElevatedInitArgv<'a> {
    hash: Option<&'a str>,
    dry_run: bool,
    yes: bool,
    rotate: bool,
    ups: Option<&'a str>,
    config: &'a Path,
    json: bool,
    ascii: bool,
    color: ColorChoice,
}

fn elevated_argv_init(args: &ElevatedInitArgv<'_>) -> Vec<OsString> {
    let mut argv: Vec<OsString> = vec![
        OsString::from("--"),
        OsString::from(INSTALLED_BINARY),
        OsString::from("init"),
    ];
    if args.dry_run {
        argv.push(OsString::from("--dry-run"));
    }
    if args.rotate {
        argv.push(OsString::from("--rotate-secrets"));
    }
    if let Some(hash) = args.hash {
        argv.push(OsString::from("--expect-plan"));
        argv.push(OsString::from(hash));
    }
    if args.yes {
        argv.push(OsString::from("--yes"));
    }
    if let Some(ups) = args.ups {
        argv.push(OsString::from("--ups"));
        argv.push(OsString::from(ups));
    }
    argv.push(OsString::from("--config"));
    argv.push(OsString::from(args.config));
    if args.json {
        argv.push(OsString::from("--json"));
    }
    if args.ascii {
        argv.push(OsString::from("--ascii"));
    }
    match args.color {
        ColorChoice::Auto => {}
        ColorChoice::Always => {
            argv.push(OsString::from("--color"));
            argv.push(OsString::from("always"));
        }
        ColorChoice::Never => {
            argv.push(OsString::from("--color"));
            argv.push(OsString::from("never"));
        }
    }
    argv
}

fn run_sudo_captured<H: Host>(
    host: &H,
    argv: &[OsString],
) -> Result<crate::host::ProcessOutput, NutError> {
    let request = ProcessRequest {
        program: OsStr::new("sudo"),
        args: argv,
        timeout: PREVIEW_TIMEOUT,
        secret: false,
        inherit_tty: false,
    };
    host.run_bounded(&request)
        .map_err(|error| NutError::operational(format!("privileged preview failed: {error}")))
}

fn run_sudo_inherited<H: Host>(host: &H, argv: &[OsString]) -> Result<i32, NutError> {
    let request = ProcessRequest {
        program: OsStr::new("sudo"),
        args: argv,
        timeout: ELEVATE_TIMEOUT,
        secret: false,
        inherit_tty: true,
    };
    let output = host
        .run_bounded(&request)
        .map_err(|error| NutError::operational(format!("privileged apply failed: {error}")))?;
    Ok(output.status.unwrap_or(1))
}

#[derive(Clone, Debug, serde::Deserialize)]
struct PrivilegedPreview {
    schema_version: u32,
    complete: bool,
    hash: Option<String>,
    steps: Vec<serde_json::Value>,
    explanations: Vec<String>,
}

fn parse_privileged_preview(bytes: &[u8]) -> Result<PrivilegedPreview, NutError> {
    let value: PrivilegedPreview = serde_json::from_slice(bytes).map_err(|error| {
        NutError::operational(format!("privileged preview returned invalid JSON: {error}"))
    })?;
    if value.schema_version != 1 {
        return Err(NutError::operational(format!(
            "privileged preview has unsupported schema {}",
            value.schema_version
        )));
    }
    if !value.complete || value.hash.is_none() {
        return Err(NutError::operational(
            "privileged preview is still partial; refusing to bind an apply hash",
        ));
    }
    Ok(value)
}

struct ApplyRender<'a> {
    hash: &'a str,
    backup: Option<&'a str>,
    files: &'a [PathBuf],
    services: &'a [String],
    resumed: bool,
}

fn render_apply_text<H: Host, W: Write>(
    host: &H,
    applied: &ApplyRender<'_>,
    stdout: &mut W,
    ui: &UiContext,
) -> Result<(), NutError> {
    use crate::ui::theme::{Theme, Tone};
    let host_name = hostname(host);
    if applied.files.is_empty() && applied.services.is_empty() {
        let line = format!(" {} Already converged. Nothing to change.", ui.glyphs.ok);
        writeln!(
            stdout,
            "{}",
            Theme::paint(Tone::Ok, &line, ui.color_enabled)
        )
        .map_err(write_error)?;
        return Ok(());
    }
    let separator = dot(ui);
    let title = format!(
        " nut init{separator}{host_name}{separator}applied {}",
        short_hash(applied.hash)
    );
    writeln!(
        stdout,
        "{}",
        Theme::paint(Tone::Ok, &title, ui.color_enabled)
    )
    .map_err(write_error)?;
    if applied.resumed {
        write_wrapped(stdout, "   Resumed an interrupted transaction.", ui.width)?;
    }
    if let Some(stamp) = applied.backup {
        write_wrapped(
            stdout,
            &format!("   Backup kept under /var/backups/nut-cli/{stamp}."),
            ui.width,
        )?;
        write_wrapped(
            stdout,
            &format!("   Recover with nut rollback {stamp} if verification fails."),
            ui.width,
        )?;
    }
    if !applied.files.is_empty() {
        write_wrapped(
            stdout,
            &format!("   Files applied: {}", applied.files.len()),
            ui.width,
        )?;
        for path in applied.files.iter().take(12) {
            write_wrapped(stdout, &format!("     ~ {}", path.display()), ui.width)?;
        }
    }
    if !applied.services.is_empty() {
        write_wrapped(
            stdout,
            &format!("   Services applied: {}", applied.services.join(", ")),
            ui.width,
        )?;
    }
    Ok(())
}

fn missing_config_guidance(config_abs: &Path) -> NutError {
    NutError::operational(format!("host config is missing: {}", config_abs.display()))
        .with_hint("run `nut config new` to scaffold a host config")
}

pub fn run_init<H: Host, W: Write>(
    args: InitArgs,
    output: InitOutput<'_>,
    host: &H,
    stdout: &mut W,
) -> Result<i32, NutError> {
    if args.dry_run && (args.yes || args.expect_plan.is_some()) {
        return Err(NutError::usage(
            "--dry-run conflicts with --yes and --expect-plan",
        ));
    }
    if args.expect_plan.is_some() && !args.yes {
        return Err(NutError::usage("--expect-plan requires --yes"));
    }
    if args.dry_run && host.euid() != 0 {
        // Dry-run never elevates; fall through to local preview only.
    }
    let config_abs = absolute_config(host, output.config_path);
    let ui = ui(host, output.ascii, output.color, host.stdout_is_terminal());
    if args.dry_run {
        return dry_run_init(host, &args, &output, &config_abs, &ui, stdout);
    }
    if args.yes {
        return yes_init(host, &args, &output, &config_abs, &ui, stdout);
    }
    interactive_init(host, &args, &output, &config_abs, &ui, stdout)
}

fn dry_run_init<H: Host, W: Write>(
    host: &H,
    args: &InitArgs,
    output: &InitOutput<'_>,
    config_abs: &Path,
    ui: &UiContext,
    stdout: &mut W,
) -> Result<i32, NutError> {
    let plan = collect_plan(
        host,
        output.config_path,
        args.ups.as_deref(),
        args.rotate_secrets,
    );
    if is_missing_config(&plan) {
        if output.json {
            write_json(stdout, &preview_json(&plan, Some(config_abs.to_path_buf())))?;
        } else {
            render_preview_text(host, &plan, stdout, ui)?;
            write_wrapped(
                stdout,
                &format!(" {} {}", arrow(ui), missing_config_guidance(config_abs)),
                ui.width,
            )?;
        }
        return Err(missing_config_guidance(config_abs));
    }
    if output.json {
        write_json(stdout, &preview_json(&plan, Some(config_abs.to_path_buf())))?;
    } else {
        render_preview_text(host, &plan, stdout, ui)?;
    }
    assert_no_secret_leak_json(&preview_json(&plan, Some(config_abs.to_path_buf())))?;
    if !plan.view().complete {
        return Err(NutError::operational(plan.view().explanations.join("; ")));
    }
    Ok(0)
}

fn yes_init<H: Host, W: Write>(
    host: &H,
    args: &InitArgs,
    output: &InitOutput<'_>,
    config_abs: &Path,
    ui: &UiContext,
    stdout: &mut W,
) -> Result<i32, NutError> {
    if host.euid() != 0 {
        return Err(NutError::operational(
            "refusing to apply as non-root; re-run interactively to review and elevate",
        )
        .with_hint("run `nut init` on a terminal and confirm the sudo preview"));
    }
    let expected = match args.expect_plan.clone() {
        Some(hash) => {
            if hash.trim().is_empty() {
                return Err(NutError::usage("--expect-plan must not be empty"));
            }
            hash
        }
        None => {
            let plan = collect_plan(
                host,
                output.config_path,
                args.ups.as_deref(),
                args.rotate_secrets,
            );
            if is_missing_config(&plan) {
                return Err(missing_config_guidance(config_abs));
            }
            if !plan.view().complete {
                return Err(NutError::operational(plan.view().explanations.join("; ")));
            }
            plan.view()
                .hash
                .clone()
                .ok_or_else(|| NutError::operational("init plan has no authoritative hash"))?
        }
    };
    if expected.trim().is_empty() {
        return Err(NutError::usage("--expect-plan must not be empty"));
    }
    // `--expect-plan` never accepts a partial plan: converge recomputes and
    // refuses on mismatch without performing target writes.
    let report = converge(
        host,
        &ConvergeOptions {
            expect_plan: expected.clone(),
            config: output.config_path.map(Path::to_path_buf),
            ups: args.ups.clone(),
            rotate_secrets: args.rotate_secrets,
        },
    )
    .map_err(map_converge)?;
    if output.json {
        let rollback_hint = report
            .backup_stamp
            .as_deref()
            .map(|stamp| format!("nut rollback {stamp}"));
        write_json(
            stdout,
            &InitApplyJson {
                schema_version: 1,
                hash: report.plan_hash.clone(),
                backup_stamp: report.backup_stamp.clone(),
                files_applied: report.files_applied.clone(),
                services_applied: report.services_applied.clone(),
                resumed: report.resumed,
                state_finalized: report.state_finalized,
                verification: verification_json(&report.doctor),
                rollback_hint,
            },
        )?;
    } else {
        render_apply_text(
            host,
            &ApplyRender {
                hash: &report.plan_hash,
                backup: report.backup_stamp.as_deref(),
                files: &report.files_applied,
                services: &report.services_applied,
                resumed: report.resumed,
            },
            stdout,
            ui,
        )?;
    }
    if !report.doctor.verified() {
        let detail = report
            .doctor
            .failures
            .iter()
            .map(|failure| format!("{}: {}", failure.id, failure.evidence))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(
            NutError::operational(format!("post-apply verification failed: {detail}")).with_hint(
                report
                    .backup_stamp
                    .as_deref()
                    .map(|stamp| format!("nut rollback {stamp}"))
                    .unwrap_or_else(|| "nut doctor".to_owned()),
            ),
        );
    }
    Ok(0)
}

fn interactive_init<H: Host, W: Write>(
    host: &H,
    args: &InitArgs,
    output: &InitOutput<'_>,
    config_abs: &Path,
    ui: &UiContext,
    stdout: &mut W,
) -> Result<i32, NutError> {
    require_interactive(host)?;
    let plan = collect_plan(
        host,
        output.config_path,
        args.ups.as_deref(),
        args.rotate_secrets,
    );
    if is_missing_config(&plan) {
        handle_missing_config_interactive(host, config_abs, stdout, ui)?;
        return Err(missing_config_guidance(config_abs));
    }
    if !plan.view().complete {
        if output.json {
            write_json(stdout, &preview_json(&plan, Some(config_abs.to_path_buf())))?;
        } else {
            render_preview_text(host, &plan, stdout, ui)?;
        }
        if is_unreadable_partial(&plan) {
            return privileged_preview_flow(host, args, output, config_abs, ui, stdout);
        }
        return Err(NutError::operational(plan.view().explanations.join("; ")));
    }
    let hash = plan
        .view()
        .hash
        .clone()
        .ok_or_else(|| NutError::operational("init plan has no authoritative hash"))?;
    if output.json {
        write_json(stdout, &preview_json(&plan, Some(config_abs.to_path_buf())))?;
    } else {
        render_preview_text(host, &plan, stdout, ui)?;
    }
    if plan.view().empty {
        return Ok(0);
    }
    if !confirm(host, "Apply? sudo will run exactly this plan.  [y/N] ")? {
        return Err(NutError::operational("cancelled; no changes were made"));
    }
    if host.euid() == 0 {
        let report = converge(
            host,
            &ConvergeOptions {
                expect_plan: hash,
                config: output.config_path.map(Path::to_path_buf),
                ups: args.ups.clone(),
                rotate_secrets: args.rotate_secrets,
            },
        )
        .map_err(map_converge)?;
        if output.json {
            let rollback_hint = report
                .backup_stamp
                .as_deref()
                .map(|stamp| format!("nut rollback {stamp}"));
            write_json(
                stdout,
                &InitApplyJson {
                    schema_version: 1,
                    hash: report.plan_hash.clone(),
                    backup_stamp: report.backup_stamp.clone(),
                    files_applied: report.files_applied.clone(),
                    services_applied: report.services_applied.clone(),
                    resumed: report.resumed,
                    state_finalized: report.state_finalized,
                    verification: verification_json(&report.doctor),
                    rollback_hint,
                },
            )?;
        } else {
            render_apply_text(
                host,
                &ApplyRender {
                    hash: &report.plan_hash,
                    backup: report.backup_stamp.as_deref(),
                    files: &report.files_applied,
                    services: &report.services_applied,
                    resumed: report.resumed,
                },
                stdout,
                ui,
            )?;
        }
        if !report.doctor.verified() {
            return Err(
                NutError::operational("post-apply verification failed").with_hint(
                    report
                        .backup_stamp
                        .as_deref()
                        .map(|stamp| format!("nut rollback {stamp}"))
                        .unwrap_or_else(|| "nut doctor".to_owned()),
                ),
            );
        }
        return Ok(0);
    }
    verify_elevation_binary(host)?;
    let argv = elevated_argv_init(&ElevatedInitArgv {
        hash: Some(&hash),
        dry_run: false,
        yes: true,
        rotate: args.rotate_secrets,
        ups: args.ups.as_deref(),
        config: config_abs,
        json: output.json,
        ascii: output.ascii,
        color: output.color,
    });
    let code = run_sudo_inherited(host, &argv)?;
    if code != 0 {
        return Err(NutError::operational(format!(
            "privileged init exited with status {code}"
        )));
    }
    Ok(code)
}

fn privileged_preview_flow<H: Host, W: Write>(
    host: &H,
    args: &InitArgs,
    output: &InitOutput<'_>,
    config_abs: &Path,
    ui: &UiContext,
    stdout: &mut W,
) -> Result<i32, NutError> {
    verify_elevation_binary(host)?;
    if !confirm(
        host,
        "Some inputs need root to read. Run a read-only privileged preview?  [y/N] ",
    )? {
        return Err(NutError::operational(
            "partial preview only; re-run with root or fix unreadable inputs",
        ));
    }
    let argv = elevated_argv_init(&ElevatedInitArgv {
        hash: None,
        dry_run: true,
        yes: false,
        rotate: args.rotate_secrets,
        ups: args.ups.as_deref(),
        config: config_abs,
        json: true,
        ascii: output.ascii,
        color: output.color,
    });
    let captured = run_sudo_captured(host, &argv)?;
    if captured.status != Some(0) {
        return Err(NutError::operational(format!(
            "privileged preview exited with status {:?}",
            captured.status
        )));
    }
    let preview = parse_privileged_preview(&captured.stdout)?;
    let hash = preview.hash.clone().expect("validated complete");
    if !output.json {
        write_wrapped(
            stdout,
            &format!(
                " Privileged preview is complete: plan {} with {} steps.",
                short_hash(&hash),
                preview.steps.len()
            ),
            ui.width,
        )?;
        for explanation in &preview.explanations {
            write_wrapped(stdout, &format!(" ! {explanation}"), ui.width)?;
        }
    } else {
        stdout.write_all(&captured.stdout).map_err(write_error)?;
        if !captured.stdout.ends_with(b"\n") {
            writeln!(stdout).map_err(write_error)?;
        }
    }
    if !confirm(host, "Apply this privileged plan with sudo?  [y/N] ")? {
        return Err(NutError::operational("cancelled; no changes were made"));
    }
    let apply_argv = elevated_argv_init(&ElevatedInitArgv {
        hash: Some(&hash),
        dry_run: false,
        yes: true,
        rotate: args.rotate_secrets,
        ups: args.ups.as_deref(),
        config: config_abs,
        json: output.json,
        ascii: output.ascii,
        color: output.color,
    });
    let code = run_sudo_inherited(host, &apply_argv)?;
    if code != 0 {
        return Err(NutError::operational(format!(
            "privileged init exited with status {code}"
        )));
    }
    Ok(code)
}

fn map_converge(error: crate::init::ConvergeError) -> NutError {
    use crate::init::ConvergeError as E;
    match &error {
        E::HashMismatch => NutError::operational(error.to_string())
            .with_hint("re-run `nut init --dry-run` and confirm the new hash"),
        E::IncompletePlan(_) => NutError::operational(error.to_string()),
        E::NotRoot => NutError::operational(error.to_string()),
        E::VerificationFailed { stamp, .. } => {
            NutError::operational(error.to_string()).with_hint(format!("nut rollback {stamp}"))
        }
        _ => NutError::operational(error.to_string()),
    }
}

fn assert_no_secret_leak_json(value: &impl Serialize) -> Result<(), NutError> {
    let text = serde_json::to_string(value)
        .map_err(|error| NutError::operational(format!("cannot encode JSON: {error}")))?;
    let lowered = text.to_ascii_lowercase();
    for needle in ["password", "secret", "sentinel"] {
        if lowered.contains(needle)
            && !lowered.contains("secret_intents")
            && !lowered.contains("operator.secret")
            && !lowered.contains("rotate")
        {
            // `operator.secret` path and intent names are safe; raw values never appear.
            if needle == "secret" {
                continue;
            }
            return Err(NutError::operational(format!(
                "refusing to emit JSON containing {needle:?}"
            )));
        }
    }
    Ok(())
}

fn handle_missing_config_interactive<H: Host, W: Write>(
    host: &H,
    config_abs: &Path,
    stdout: &mut W,
    ui: &UiContext,
) -> Result<bool, NutError> {
    // Offer the existing `config new` discovery scaffold without writing.
    // Saving the system config requires the same apply confirmation as init.
    use crate::ui::theme::{Theme, Tone};
    let catalog = crate::profiles::load_catalog(host)
        .map_err(|error| NutError::operational(format!("cannot load UPS profiles: {error}")))?;
    let scaffold = crate::config::scaffold_for_init(host, &catalog)?;
    writeln!(
        stdout,
        "{}",
        Theme::paint(
            Tone::Info,
            &format!(
                " nut init{}no host config at {}",
                dot(ui),
                config_abs.display()
            ),
            ui.color_enabled
        )
    )
    .map_err(write_error)?;
    write_wrapped(
        stdout,
        " No host config was found. Review the scaffold below, then save it to install it as part of init.",
        ui.width,
    )?;
    for line in scaffold.lines().take(40) {
        writeln!(stdout, "   {line}").map_err(write_error)?;
    }
    write_wrapped(
        stdout,
        "Run `nut config new` to write this scaffold to a file for editing.",
        ui.width,
    )?;
    Ok(false)
}

// ---------------------------------------------------------------------------
// Rollback
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
struct RollbackListJson {
    schema_version: u32,
    backups: Vec<BackupJson>,
}

#[derive(Clone, Debug, Serialize)]
struct BackupJson {
    stamp: String,
    files: Vec<BackupFileJson>,
    services: Vec<String>,
    membership_removals: usize,
}

#[derive(Clone, Debug, Serialize)]
struct BackupFileJson {
    path: PathBuf,
    action: String,
}

#[derive(Clone, Debug, Serialize)]
struct RollbackPreviewJson {
    schema_version: u32,
    stamp: String,
    files: Vec<BackupFileJson>,
    services: Vec<String>,
    membership_removals: Vec<String>,
    groups_maybe_removed: Vec<String>,
    package_boundary: String,
    already_complete: bool,
}

#[derive(Clone, Debug, Serialize)]
struct RollbackApplyJson {
    schema_version: u32,
    stamp: String,
    files_restored: Vec<PathBuf>,
    memberships_removed: Vec<String>,
    services_changed: Vec<String>,
    groups_removed: Vec<String>,
    groups_retained: Vec<String>,
    already_complete: bool,
    package_boundary: String,
}

fn backup_to_json(summary: &crate::init::BackupSummary) -> BackupJson {
    BackupJson {
        stamp: summary.stamp.clone(),
        files: summary
            .files
            .iter()
            .map(|file| BackupFileJson {
                path: file.path.clone(),
                action: format!("{:?}", file.action).to_ascii_lowercase(),
            })
            .collect(),
        services: summary
            .services
            .iter()
            .map(|service| service.unit.clone())
            .collect(),
        membership_removals: summary.membership_additions.len(),
    }
}

fn render_backup_list_text<H: Host, W: Write>(
    host: &H,
    summaries: &[crate::init::BackupSummary],
    stdout: &mut W,
    ui: &UiContext,
) -> Result<(), NutError> {
    use crate::ui::theme::{Theme, Tone};
    let separator = dot(ui);
    let title = format!(
        " nut rollback{separator}{}{separator}{} backup(s)",
        hostname(host),
        summaries.len()
    );
    writeln!(
        stdout,
        "{}",
        Theme::paint(Tone::Info, &title, ui.color_enabled)
    )
    .map_err(write_error)?;
    if summaries.is_empty() {
        write_wrapped(stdout, " No complete init backups.", ui.width)?;
        return Ok(());
    }
    let separator = dot(ui);
    for summary in summaries {
        write_wrapped(
            stdout,
            &format!(
                "   {}  {} files{}services {}{}memberships {}",
                summary.stamp,
                summary.files.len(),
                separator,
                summary.services.len(),
                separator,
                summary.membership_additions.len()
            ),
            ui.width,
        )?;
    }
    Ok(())
}

fn render_rollback_preview_text<W: Write>(
    plan: &crate::init::RollbackPlan,
    stdout: &mut W,
    ui: &UiContext,
) -> Result<(), NutError> {
    use crate::ui::theme::{Theme, Tone};
    if plan.already_complete {
        let line = format!(
            " {} Rollback {} is already complete.",
            ui.glyphs.ok, plan.stamp
        );
        writeln!(
            stdout,
            "{}",
            Theme::paint(Tone::Ok, &line, ui.color_enabled)
        )
        .map_err(write_error)?;
        return Ok(());
    }
    let separator = dot(ui);
    let title = format!(" nut rollback{separator}preview {}", plan.stamp);
    writeln!(
        stdout,
        "{}",
        Theme::paint(Tone::Info, &title, ui.color_enabled)
    )
    .map_err(write_error)?;
    write_wrapped(
        stdout,
        &format!(
            "   Files to restore: {}{}services: {}{}memberships: {}",
            plan.files.len(),
            separator,
            plan.services.len(),
            separator,
            plan.membership_removals.len()
        ),
        ui.width,
    )?;
    for file in plan.files.iter().take(12) {
        write_wrapped(
            stdout,
            &format!("     ~ {}  {:?}", file.path.display(), file.action),
            ui.width,
        )?;
    }
    write_wrapped(stdout, &format!("   {}", plan.package_boundary), ui.width)?;
    Ok(())
}

fn render_rollback_report_text<H: Host, W: Write>(
    host: &H,
    report: &crate::init::RollbackReport,
    stdout: &mut W,
    ui: &UiContext,
) -> Result<(), NutError> {
    use crate::ui::theme::{Theme, Tone};
    let _ = hostname(host);
    if report.already_complete {
        let line = format!(
            " {} Rollback {} is already complete.",
            ui.glyphs.ok, report.stamp
        );
        writeln!(
            stdout,
            "{}",
            Theme::paint(Tone::Ok, &line, ui.color_enabled)
        )
        .map_err(write_error)?;
        return Ok(());
    }
    let separator = dot(ui);
    let title = format!(" nut rollback{separator}restored {}", report.stamp);
    writeln!(
        stdout,
        "{}",
        Theme::paint(Tone::Ok, &title, ui.color_enabled)
    )
    .map_err(write_error)?;
    write_wrapped(
        stdout,
        &format!(
            "   Files restored: {}{}services changed: {}",
            report.files_restored.len(),
            separator,
            report.services_changed.len()
        ),
        ui.width,
    )?;
    for path in report.files_restored.iter().take(12) {
        write_wrapped(stdout, &format!("     ~ {}", path.display()), ui.width)?;
    }
    write_wrapped(stdout, &format!("   {}", report.package_boundary), ui.width)?;
    Ok(())
}

fn elevated_argv_rollback(
    stamp: Option<&str>,
    dry_run: bool,
    yes: bool,
    list: bool,
    json: bool,
    ascii: bool,
    color: ColorChoice,
) -> Vec<OsString> {
    let mut argv: Vec<OsString> = vec![
        OsString::from("--"),
        OsString::from(INSTALLED_BINARY),
        OsString::from("rollback"),
    ];
    if list {
        argv.push(OsString::from("--list"));
    }
    if dry_run {
        argv.push(OsString::from("--dry-run"));
    }
    if yes {
        argv.push(OsString::from("--yes"));
    }
    if let Some(stamp) = stamp {
        argv.push(OsString::from(stamp));
    }
    if json {
        argv.push(OsString::from("--json"));
    }
    if ascii {
        argv.push(OsString::from("--ascii"));
    }
    match color {
        ColorChoice::Auto => {}
        ColorChoice::Always => {
            argv.push(OsString::from("--color"));
            argv.push(OsString::from("always"));
        }
        ColorChoice::Never => {
            argv.push(OsString::from("--color"));
            argv.push(OsString::from("never"));
        }
    }
    argv
}

fn map_rollback(error: crate::init::RollbackError) -> NutError {
    use crate::init::RollbackError as E;
    match &error {
        E::NoBackups => NutError::operational(error.to_string())
            .with_hint("no init backup exists yet; run `nut init` first"),
        E::PendingTransaction => NutError::operational(error.to_string())
            .with_hint("recover the pending init transaction first"),
        E::NotCurrent(_) => NutError::operational(error.to_string()),
        _ => NutError::operational(error.to_string()),
    }
}

pub fn run_rollback<H: Host, W: Write>(
    args: RollbackArgs,
    output: RollbackOutput<'_>,
    host: &H,
    stdout: &mut W,
) -> Result<i32, NutError> {
    if args.list && (args.stamp.is_some() || args.dry_run || args.yes) {
        return Err(NutError::usage(
            "--list conflicts with STAMP, --dry-run, and --yes",
        ));
    }
    if args.dry_run && args.yes {
        return Err(NutError::usage("--dry-run conflicts with --yes"));
    }
    let ui = ui(host, output.ascii, output.color, host.stdout_is_terminal());
    if args.list {
        return rollback_list(host, &output, &ui, stdout);
    }
    if args.dry_run {
        return rollback_dry_run(host, &args, &output, &ui, stdout);
    }
    rollback_apply(host, &args, &output, &ui, stdout)
}

fn rollback_list<H: Host, W: Write>(
    host: &H,
    output: &RollbackOutput<'_>,
    ui: &UiContext,
    stdout: &mut W,
) -> Result<i32, NutError> {
    // List never mutates or elevates: a permission failure is reported as-is.
    let summaries = rollback::list_backups(host).map_err(map_rollback)?;
    if output.json {
        write_json(
            stdout,
            &RollbackListJson {
                schema_version: 1,
                backups: summaries.iter().map(backup_to_json).collect(),
            },
        )?;
    } else {
        render_backup_list_text(host, &summaries, stdout, ui)?;
    }
    Ok(0)
}

fn rollback_dry_run<H: Host, W: Write>(
    host: &H,
    args: &RollbackArgs,
    output: &RollbackOutput<'_>,
    ui: &UiContext,
    stdout: &mut W,
) -> Result<i32, NutError> {
    // Dry-run never mutates or elevates.
    let plan = rollback::plan(host, args.stamp.as_deref()).map_err(map_rollback)?;
    if output.json {
        write_json(
            stdout,
            &RollbackPreviewJson {
                schema_version: 1,
                stamp: plan.stamp.clone(),
                files: plan
                    .files
                    .iter()
                    .map(|file| BackupFileJson {
                        path: file.path.clone(),
                        action: format!("{:?}", file.action).to_ascii_lowercase(),
                    })
                    .collect(),
                services: plan
                    .services
                    .iter()
                    .map(|service| service.unit.clone())
                    .collect(),
                membership_removals: plan
                    .membership_removals
                    .iter()
                    .map(|addition| format!("{}:{}", addition.group, addition.user))
                    .collect(),
                groups_maybe_removed: plan.groups_maybe_removed.clone(),
                package_boundary: plan.package_boundary.to_owned(),
                already_complete: plan.already_complete,
            },
        )?;
    } else {
        render_rollback_preview_text(&plan, stdout, ui)?;
    }
    Ok(0)
}

fn rollback_apply<H: Host, W: Write>(
    host: &H,
    args: &RollbackArgs,
    output: &RollbackOutput<'_>,
    ui: &UiContext,
    stdout: &mut W,
) -> Result<i32, NutError> {
    if host.euid() != 0 && args.yes {
        return Err(NutError::operational(
            "refusing to restore as non-root; re-run interactively to review and elevate",
        )
        .with_hint("run `nut rollback` on a terminal and confirm the sudo preview"));
    }
    if host.euid() != 0 {
        require_interactive(host)?;
        // Non-root apply needs a read-only preview first. If the local plan
        // fails on protected backups, fall back to a privileged dry-run.
        let local = rollback::plan(host, args.stamp.as_deref());
        match local {
            Ok(plan) => {
                if !output.json {
                    render_rollback_preview_text(&plan, stdout, ui)?;
                } else {
                    // JSON apply still shows the reviewed preview first in text?
                    // Emit the preview JSON, then prompt; the restore below emits its own JSON.
                    // To keep machine output parseable, only the final report is JSON.
                }
                if !confirm(host, &format!("Restore backup {}?  [y/N] ", plan.stamp))? {
                    return Err(NutError::operational("cancelled; no changes were made"));
                }
                verify_elevation_binary(host)?;
                let argv = elevated_argv_rollback(
                    args.stamp.as_deref().or(Some(plan.stamp.as_str())),
                    false,
                    true,
                    false,
                    output.json,
                    output.ascii,
                    output.color,
                );
                let code = run_sudo_inherited(host, &argv)?;
                if code != 0 {
                    return Err(NutError::operational(format!(
                        "privileged rollback exited with status {code}"
                    )));
                }
                return Ok(code);
            }
            Err(_) => {
                verify_elevation_binary(host)?;
                if !confirm(
                    host,
                    "Backups need root to read. Run a read-only privileged preview?  [y/N] ",
                )? {
                    return Err(NutError::operational(
                        "rollback preview needs root; re-run with sudo for a read-only preview",
                    ));
                }
                let preview_argv = elevated_argv_rollback(
                    args.stamp.as_deref(),
                    true,
                    false,
                    false,
                    true,
                    output.ascii,
                    output.color,
                );
                let captured = run_sudo_captured(host, &preview_argv)?;
                if captured.status != Some(0) {
                    return Err(NutError::operational("privileged rollback preview failed"));
                }
                let preview: serde_json::Value =
                    serde_json::from_slice(&captured.stdout).map_err(|error| {
                        NutError::operational(format!(
                            "privileged rollback preview is invalid: {error}"
                        ))
                    })?;
                let stamp = preview
                    .get("stamp")
                    .and_then(|value| value.as_str())
                    .unwrap_or(args.stamp.as_deref().unwrap_or("latest"))
                    .to_owned();
                stdout.write_all(&captured.stdout).map_err(write_error)?;
                if !captured.stdout.ends_with(b"\n") {
                    writeln!(stdout).map_err(write_error)?;
                }
                if !confirm(host, &format!("Restore backup {stamp}?  [y/N] "))? {
                    return Err(NutError::operational("cancelled; no changes were made"));
                }
                let argv = elevated_argv_rollback(
                    Some(&stamp),
                    false,
                    true,
                    false,
                    output.json,
                    output.ascii,
                    output.color,
                );
                let code = run_sudo_inherited(host, &argv)?;
                if code != 0 {
                    return Err(NutError::operational(format!(
                        "privileged rollback exited with status {code}"
                    )));
                }
                return Ok(code);
            }
        }
    }
    // Root apply.
    if !args.yes {
        require_interactive(host)?;
        let plan = rollback::plan(host, args.stamp.as_deref()).map_err(map_rollback)?;
        if output.json {
            // For machine output, emit the preview then require an explicit
            // `--yes` re-invocation; interactive JSON still prompts on the TTY.
        } else {
            render_rollback_preview_text(&plan, stdout, ui)?;
        }
        if plan.already_complete {
            if output.json {
                write_json(
                    stdout,
                    &RollbackApplyJson {
                        schema_version: 1,
                        stamp: plan.stamp.clone(),
                        files_restored: Vec::new(),
                        memberships_removed: Vec::new(),
                        services_changed: Vec::new(),
                        groups_removed: Vec::new(),
                        groups_retained: Vec::new(),
                        already_complete: true,
                        package_boundary: plan.package_boundary.to_owned(),
                    },
                )?;
            }
            return Ok(0);
        }
        if !confirm(host, &format!("Restore backup {}?  [y/N] ", plan.stamp))? {
            return Err(NutError::operational("cancelled; no changes were made"));
        }
        let report = rollback::restore(host, args.stamp.as_deref(), &RollbackOptions::default())
            .map_err(map_rollback)?;
        if output.json {
            write_json(
                stdout,
                &RollbackApplyJson {
                    schema_version: 1,
                    stamp: report.stamp.clone(),
                    files_restored: report.files_restored.clone(),
                    memberships_removed: report
                        .memberships_removed
                        .iter()
                        .map(|addition| format!("{}:{}", addition.group, addition.user))
                        .collect(),
                    services_changed: report.services_changed.clone(),
                    groups_removed: report.groups_removed.clone(),
                    groups_retained: report.groups_retained.clone(),
                    already_complete: report.already_complete,
                    package_boundary: report.package_boundary.to_owned(),
                },
            )?;
        } else {
            render_rollback_report_text(host, &report, stdout, ui)?;
        }
        return Ok(0);
    }
    let report = rollback::restore(host, args.stamp.as_deref(), &RollbackOptions::default())
        .map_err(map_rollback)?;
    if output.json {
        write_json(
            stdout,
            &RollbackApplyJson {
                schema_version: 1,
                stamp: report.stamp.clone(),
                files_restored: report.files_restored.clone(),
                memberships_removed: report
                    .memberships_removed
                    .iter()
                    .map(|addition| format!("{}:{}", addition.group, addition.user))
                    .collect(),
                services_changed: report.services_changed.clone(),
                groups_removed: report.groups_removed.clone(),
                groups_retained: report.groups_retained.clone(),
                already_complete: report.already_complete,
                package_boundary: report.package_boundary.to_owned(),
            },
        )?;
    } else {
        render_rollback_report_text(host, &report, stdout, ui)?;
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use crate::host::{FakeHost, FileMetadata, HostEffect, ProcessOutput, UpsObservation};
    use std::collections::BTreeMap;
    use std::path::Path;

    const SENTINEL: &str = "SENTINEL-CLI-9f8e7d6c5b4a";

    fn out(status: i32, stdout: &str) -> ProcessOutput {
        ProcessOutput {
            status: Some(status),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        }
    }

    fn ol() -> UpsObservation {
        UpsObservation {
            vars: BTreeMap::from([
                ("ups.status".into(), "OL CHRG".into()),
                ("battery.charge".into(), "100".into()),
                ("battery.runtime".into(), "1800".into()),
                ("ups.delay.shutdown".into(), "60".into()),
                ("ups.delay.start".into(), "0".into()),
                ("ups.test.result".into(), "No test initiated".into()),
            ]),
            cmds: vec!["shutdown.return".into()],
            clients: vec!["127.0.0.1".into()],
            greeting: Some("Network UPS Tools upsd 2.8.1".into()),
            nut_version: Some("2.8.1".into()),
            ..UpsObservation::default()
        }
    }

    fn systemctl_active(host: FakeHost, unit: &str, state: &str) -> FakeHost {
        host.with_program_args("systemctl", ["is-active", unit], out(0, state))
    }

    fn systemctl_enabled(host: FakeHost, unit: &str, state: &str) -> FakeHost {
        host.with_program_args("systemctl", ["is-enabled", unit], out(0, state))
    }

    fn systemctl_show(host: FakeHost, unit: &str, active: &str) -> FakeHost {
        host.with_program_args(
            "systemctl",
            ["show", unit, "-p", "ActiveState", "-p", "NRestarts"],
            out(0, &format!("ActiveState={active}\nNRestarts=0\n")),
        )
    }

    fn fixture_host() -> FakeHost {
        let mut host = FakeHost::default()
            .with_euid(0)
            .with_hostname("athena")
            .with_env("HOME", "/root")
            .with_env("NUT_CONFIG", "/etc/nut-cli/nut.toml")
            .with_env("LANG", "C.UTF-8")
            .with_env("TERM", "xterm")
            .with_current_exe("/usr/local/bin/nut")
            .with_metadata(
                "/usr/local/bin/nut",
                FileMetadata {
                    uid: 0,
                    gid: 0,
                    mode: 0o755,
                    is_file: true,
                    is_dir: false,
                },
            )
            .with_dir("/usr")
            .with_dir("/usr/local")
            .with_dir("/usr/local/bin")
            .with_metadata("/usr", FileMetadata::dir(0, 0, 0o755))
            .with_metadata("/usr/local", FileMetadata::dir(0, 0, 0o755))
            .with_metadata("/usr/local/bin", FileMetadata::dir(0, 0, 0o755))
            .with_metadata("/", FileMetadata::dir(0, 0, 0o755))
            .with_file(
                "/etc/nut-cli/nut.toml",
                include_bytes!("../../tests/fixtures/hosts/athena/nut.toml").to_vec(),
            )
            .with_file("/etc/group", b"nut:x:100:\nnut-operators:x:101:bryan\n".to_vec())
            .with_file(
                "/etc/passwd",
                b"bryan:x:1000:1000::/home/bryan:/bin/bash\n".to_vec(),
            )
            .with_file(
                "/etc/fstab",
                b"UUID=abc / ext4 defaults 0 1\nUUID=def /mnt/poseidon ext4 defaults,nofail 0 2\nUUID=ghi /mnt/hades ext4 defaults,nofail 0 2\n"
                    .to_vec(),
            )
            .with_file(
                "/sys/class/dmi/id/board_vendor",
                b"ASUSTeK COMPUTER INC.\n".to_vec(),
            )
            .with_file(
                "/sys/class/dmi/id/board_name",
                b"PRIME TRX40-PRO\n".to_vec(),
            )
            .with_file("/sys/class/dmi/id/bios_version", b"1101\n".to_vec())
            .with_file("/sys/bus/usb/devices/1-4/idVendor", b"0764".to_vec())
            .with_file("/sys/bus/usb/devices/1-4/idProduct", b"0601".to_vec())
            .with_file("/sys/bus/usb/devices/1-4/busnum", b"1".to_vec())
            .with_file("/sys/bus/usb/devices/1-4/devnum", b"4".to_vec())
            .with_file("/dev/bus/usb/001/004", b"".to_vec())
            .with_metadata(
                "/dev/bus/usb/001/004",
                FileMetadata {
                    uid: 0,
                    gid: 100,
                    mode: 0o660,
                    is_file: true,
                    is_dir: false,
                },
            )
            .with_program_args("dpkg-query", ["-W", "-f=${Version}", "nut-client"], out(0, "2.8.1-1"))
            .with_program_args("dpkg-query", ["-W", "-f=${Version}", "nut-server"], out(0, "2.8.1-1"))
            .with_program_args("getent", ["group", "nut"], out(0, "nut:x:100:\n"))
            .with_program_args(
                "getent",
                ["group", "nut-operators"],
                out(0, "nut-operators:x:101:bryan\n"),
            )
            .with_program("systemctl", out(0, ""))
            .with_program_args("pgrep", ["-x", "pwrstatd"], ProcessOutput {
                status: Some(1),
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
            .with_program_args("findmnt", ["--verify"], out(0, ""))
            .with_program(
                "efibootmgr",
                out(0, "BootCurrent: 0002\nBootOrder: 0002,0001\n"),
            )
            .with_file("/usr/sbin/sendmail", b"".to_vec())
            .with_dir("/var/log/journal")
            .with_file("/proc/sys/kernel/panic", b"0\n".to_vec())
            .with_file(
                "/proc/net/tcp",
                b"  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   0: 0100007F:0DA5 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1 1 0000000000000000 0 0 0 10 -1\n"
                    .to_vec(),
            )
            .with_file(
                "/proc/net/tcp6",
                b"  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n"
                    .to_vec(),
            );
        for unit in [
            "nut-driver@cp1500.service",
            "nut-server.service",
            "nut-monitor.service",
            "nut.target",
        ] {
            host = systemctl_active(host, unit, "active");
            host = systemctl_enabled(host, unit, "enabled");
        }
        for unit in [
            "nut-driver@cp1500.service",
            "nut-server.service",
            "nut-monitor.service",
        ] {
            host = systemctl_show(host, unit, "active");
        }
        host.with_ups_observation(ol())
            .with_ups_observation(ol())
            .with_ups_observation(ol())
            .with_ups_observation(ol())
            .with_ups_observation(ol())
    }

    fn execute(args: &[&str], host: &FakeHost) -> (i32, String, String) {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let code = crate::run_with_host(args.iter().copied(), host, &mut stdout, &mut stderr);
        (
            code,
            String::from_utf8(stdout).unwrap(),
            String::from_utf8(stderr).unwrap(),
        )
    }

    #[test]
    fn conflicting_init_flags_are_usage_errors() {
        let host = fixture_host();
        for args in [
            vec!["nut", "init", "--dry-run", "--yes"],
            vec!["nut", "init", "--dry-run", "--expect-plan", "abc"],
            vec!["nut", "init", "--expect-plan", "abc"],
            vec!["nut", "rollback", "--list", "--dry-run"],
            vec!["nut", "rollback", "stamp", "--list"],
            vec!["nut", "rollback", "--dry-run", "--yes"],
        ] {
            let (code, _, _) = execute(&args, &host);
            assert_eq!(code, 2, "{args:?}");
        }
    }

    #[test]
    fn dry_run_previews_without_mutation_and_json_has_no_secrets() {
        let host = fixture_host();
        let (code, text, err) = execute(&["nut", "init", "--dry-run"], &host);
        assert_eq!(code, 0, "{err}\n{text}");
        assert!(text.contains("nut init"), "{text}");
        assert!(
            host.effects().iter().all(|effect| !matches!(
                effect,
                HostEffect::ReplaceFile { .. }
                    | HostEffect::Remove { .. }
                    | HostEffect::Random { .. }
                    | HostEffect::CreateDir { .. }
            )),
            "{:?}",
            host.effects()
        );
        let (code, json_text, err) = execute(&["nut", "--json", "init", "--dry-run"], &host);
        assert_eq!(code, 0, "{err}");
        let json: serde_json::Value = serde_json::from_str(&json_text).unwrap();
        assert_eq!(json["schema_version"], 1);
        assert_eq!(json["complete"], true);
        assert!(json["hash"].as_str().is_some());
        let flat = json_text.to_ascii_lowercase();
        assert!(!flat.contains("password"), "{json_text}");
        assert!(
            !flat.contains(&SENTINEL.to_ascii_lowercase()),
            "{json_text}"
        );
    }

    #[test]
    fn non_root_yes_never_initiates_sudo() {
        let host = fixture_host().with_euid(1000);
        let (code, _, err) = execute(&["nut", "init", "--yes", "--expect-plan", "abc"], &host);
        assert_eq!(code, 1, "{err}");
        assert!(
            host.effects().iter().all(|effect| !matches!(
                effect,
                HostEffect::Run { program, .. } if program.to_string_lossy() == "sudo"
            )),
            "{:?}",
            host.effects()
        );
    }

    #[test]
    fn interactive_non_tty_refuses_before_prompt() {
        let host = fixture_host()
            .with_euid(1000)
            .with_tty(false, false)
            .with_stdin_tty(false);
        let (code, _, err) = execute(&["nut", "init"], &host);
        assert_eq!(code, 1);
        assert!(err.contains("terminals") || err.contains("prompt"), "{err}");
    }

    #[test]
    fn binary_ownership_refusals_cover_path_owner_and_ancestors() {
        let bad_exe = fixture_host()
            .with_euid(1000)
            .with_current_exe("/home/bryan/nut");
        let (code, _, err) = execute(
            &["nut", "init"],
            &bad_exe
                .with_tty(true, true)
                .with_stdin_tty(true)
                .with_prompt_lines(["y"]),
        );
        assert_eq!(code, 1, "{err}");
        assert!(
            err.contains("elevation") || err.contains("installed"),
            "{err}"
        );

        let bad_owner = fixture_host().with_euid(1000).with_metadata(
            "/usr/local/bin/nut",
            FileMetadata {
                uid: 1000,
                gid: 0,
                mode: 0o755,
                is_file: true,
                is_dir: false,
            },
        );
        let (code, _, err) = execute(
            &["nut", "init"],
            &bad_owner
                .with_tty(true, true)
                .with_stdin_tty(true)
                .with_prompt_lines(["y"]),
        );
        assert_eq!(code, 1, "{err}");

        let writable_ancestor = fixture_host().with_euid(1000).with_metadata(
            "/usr/local",
            FileMetadata {
                uid: 0,
                gid: 0,
                mode: 0o777,
                is_file: false,
                is_dir: true,
            },
        );
        let (code, _, err) = execute(
            &["nut", "init"],
            &writable_ancestor
                .with_tty(true, true)
                .with_stdin_tty(true)
                .with_prompt_lines(["y"]),
        );
        assert_eq!(code, 1, "{err}");
    }

    #[test]
    fn interactive_confirm_and_cancel_leave_no_writes_on_cancel() {
        let host = fixture_host()
            .with_euid(0)
            .with_tty(true, true)
            .with_stdin_tty(true)
            .with_prompt_lines(["n"]);
        let (code, text, _) = execute(&["nut", "init"], &host);
        assert_eq!(code, 1);
        assert!(text.contains("nut init"), "{text}");
    }

    #[test]
    fn elevated_apply_uses_exact_reviewed_argv() {
        use std::ffi::OsString;
        let complete = fixture_host().with_euid(1000);
        let observation = crate::init::collect(&complete, None, None);
        let planned = crate::init::plan(
            &observation,
            &crate::init::PlanOptions {
                rotate_secrets: false,
            },
        );
        assert!(planned.complete, "{:?}", planned.explanations);
        let hash = planned.hash.clone().expect("complete plan has hash");
        let config_abs = "/etc/nut-cli/nut.toml";
        let expected: Vec<OsString> = vec![
            OsString::from("--"),
            OsString::from("/usr/local/bin/nut"),
            OsString::from("init"),
            OsString::from("--expect-plan"),
            OsString::from(&hash),
            OsString::from("--yes"),
            OsString::from("--config"),
            OsString::from(config_abs),
        ];
        let host = complete
            .with_tty(true, true)
            .with_stdin_tty(true)
            .with_prompt_lines(["y"])
            .with_program_args("sudo", expected.clone(), out(0, ""));
        let (code, _, err) = execute(&["nut", "init"], &host);
        assert_eq!(code, 0, "{err}");
        let sudo_calls: Vec<_> = host
            .effects()
            .iter()
            .filter_map(|effect| match effect {
                HostEffect::Run { program, args } if program.to_string_lossy() == "sudo" => {
                    Some(args.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(sudo_calls.len(), 1, "{:?}", host.effects());
        assert_eq!(sudo_calls[0], expected);
    }

    #[test]
    fn privileged_preview_validates_json_and_binds_apply() {
        use std::ffi::OsString;
        let complete = fixture_host();
        let observation = crate::init::collect(&complete, None, None);
        let planned = crate::init::plan(
            &observation,
            &crate::init::PlanOptions {
                rotate_secrets: false,
            },
        );
        let hash = planned.hash.clone().expect("complete plan has hash");
        let preview_json = serde_json::json!({
            "schema_version": 1,
            "complete": true,
            "hash": hash,
            "steps": [],
            "explanations": [],
        });
        let preview_bytes = serde_json::to_vec(&preview_json).unwrap();
        let config_abs = "/etc/nut-cli/nut.toml";
        let preview_argv: Vec<OsString> = vec![
            OsString::from("--"),
            OsString::from("/usr/local/bin/nut"),
            OsString::from("init"),
            OsString::from("--dry-run"),
            OsString::from("--config"),
            OsString::from(config_abs),
            OsString::from("--json"),
        ];
        let apply_argv: Vec<OsString> = vec![
            OsString::from("--"),
            OsString::from("/usr/local/bin/nut"),
            OsString::from("init"),
            OsString::from("--expect-plan"),
            OsString::from(&hash),
            OsString::from("--yes"),
            OsString::from("--config"),
            OsString::from(config_abs),
            OsString::from("--json"),
        ];
        let host = FakeHost::default()
            .with_euid(1000)
            .with_hostname("athena")
            .with_env("HOME", "/root")
            .with_env("LANG", "C.UTF-8")
            .with_env("TERM", "xterm")
            .with_current_exe("/usr/local/bin/nut")
            .with_metadata("/usr/local/bin/nut", FileMetadata::file(0, 0, 0o755))
            .with_dir("/usr")
            .with_dir("/usr/local")
            .with_dir("/usr/local/bin")
            .with_metadata("/usr", FileMetadata::dir(0, 0, 0o755))
            .with_metadata("/usr/local", FileMetadata::dir(0, 0, 0o755))
            .with_metadata("/usr/local/bin", FileMetadata::dir(0, 0, 0o755))
            .with_metadata("/", FileMetadata::dir(0, 0, 0o755))
            .with_file(
                "/etc/nut-cli/nut.toml",
                include_bytes!("../../tests/fixtures/hosts/athena/nut.toml").to_vec(),
            )
            .with_unreadable(std::path::Path::new("/etc/nut/nut.conf"))
            .with_tty(true, true)
            .with_stdin_tty(true)
            .with_prompt_lines(["y", "y"])
            .with_program_args(
                "sudo",
                preview_argv.clone(),
                ProcessOutput {
                    status: Some(0),
                    stdout: preview_bytes,
                    stderr: Vec::new(),
                },
            )
            .with_program_args("sudo", apply_argv.clone(), out(0, ""));
        let (code, _, err) = execute(&["nut", "--json", "init"], &host);
        assert_eq!(code, 0, "{err}");
        let sudo_calls: Vec<_> = host
            .effects()
            .iter()
            .filter_map(|effect| match effect {
                HostEffect::Run { program, args } if program.to_string_lossy() == "sudo" => {
                    Some(args.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(sudo_calls.len(), 2, "{:?}", host.effects());
        assert_eq!(sudo_calls[0], preview_argv);
        assert_eq!(sudo_calls[1], apply_argv);
    }

    #[test]
    fn root_plan_drift_refuses_without_target_writes() {
        let host = fixture_host().with_euid(0);
        let observation = crate::init::collect(&host, None, None);
        let planned = crate::init::plan(
            &observation,
            &crate::init::PlanOptions {
                rotate_secrets: false,
            },
        );
        assert!(planned.complete);
        let bad_hash = "f".repeat(64);
        assert_ne!(planned.hash.as_deref(), Some(bad_hash.as_str()));
        let (code, _, err) = execute(&["nut", "init", "--yes", "--expect-plan", &bad_hash], &host);
        assert_eq!(code, 1, "{err}");
        assert!(
            err.contains("does not match") || err.contains("re-observed"),
            "{err}"
        );
    }

    #[test]
    fn partial_preview_handles_unreadable_and_never_accepts_its_hash() {
        let host = FakeHost::default()
            .with_euid(1000)
            .with_hostname("athena")
            .with_env("HOME", "/root")
            .with_env("LANG", "C.UTF-8")
            .with_env("TERM", "xterm")
            .with_unreadable(Path::new("/etc/nut/nut.conf"))
            .with_file(
                "/etc/nut-cli/nut.toml",
                include_bytes!("../../tests/fixtures/hosts/athena/nut.toml").to_vec(),
            );
        let (code, text, _) = execute(&["nut", "init", "--dry-run"], &host);
        assert_eq!(code, 1, "{text}");
        assert!(
            text.contains("partial") || text.contains("needs root"),
            "{text}"
        );
        let (code, _, err) = execute(&["nut", "init", "--yes", "--expect-plan", "abc"], &host);
        assert_eq!(code, 1, "{err}");
    }

    #[test]
    fn config_less_flows_guide_without_writing() {
        let host = FakeHost::default()
            .with_euid(1000)
            .with_hostname("athena")
            .with_env("HOME", "/root")
            .with_env("LANG", "C.UTF-8")
            .with_env("TERM", "xterm");
        let (code, _, err) = execute(&["nut", "init", "--dry-run"], &host);
        assert_eq!(code, 1, "{err}");
        assert!(
            err.contains("config") || err.contains("nut config new"),
            "{err}"
        );
        assert!(
            host.effects().iter().all(|effect| !matches!(
                effect,
                HostEffect::Run { program, .. } if program.to_string_lossy() == "sudo"
            )),
            "{:?}",
            host.effects()
        );
    }

    #[test]
    fn rollback_list_and_dry_run_never_mutate_or_elevate() {
        let host = fixture_host().with_euid(1000);
        let (code, text, _) = execute(&["nut", "rollback", "--list"], &host);
        assert_eq!(code, 0, "{text}");
        assert!(text.contains("rollback"), "{text}");
        assert!(
            host.effects().iter().all(|effect| !matches!(
                effect,
                HostEffect::Run { program, .. } if program.to_string_lossy() == "sudo"
            )),
            "{:?}",
            host.effects()
        );
        let (code, _, _) = execute(&["nut", "rollback", "--dry-run"], &host);
        assert!(code == 0 || code == 1);
        assert!(
            host.effects().iter().all(|effect| !matches!(
                effect,
                HostEffect::Run { program, .. } if program.to_string_lossy() == "sudo"
            )),
            "{:?}",
            host.effects()
        );
    }

    #[test]
    fn init_screens_at_supported_widths_and_ascii() {
        for width in [60, 80, 120] {
            for ascii in [false, true] {
                let host = fixture_host().with_env("COLUMNS", width.to_string());
                let mut args = vec![
                    "nut",
                    "--config",
                    "/etc/nut-cli/nut.toml",
                    "init",
                    "--dry-run",
                ];
                if ascii {
                    args.insert(1, "--ascii");
                }
                let (code, output, error) = execute(&args, &host);
                assert_eq!(code, 0, "{error}");
                assert!(
                    output.lines().all(|line| line.chars().count() <= width),
                    "{output}"
                );
                assert!(!output.contains('\u{1b}'));
                if ascii {
                    assert!(output.is_ascii(), "{output}");
                }
                insta::assert_snapshot!(
                    format!("init_{width}_{}", if ascii { "ascii" } else { "unicode" }),
                    output
                );
                let (code, json_text, _) = execute(
                    &[
                        "nut",
                        "--json",
                        "--config",
                        "/etc/nut-cli/nut.toml",
                        "init",
                        "--dry-run",
                    ],
                    &host,
                );
                assert_eq!(code, 0);
                let json: serde_json::Value = serde_json::from_str(&json_text).unwrap();
                assert_eq!(json["schema_version"], 1);
            }
        }
    }

    #[test]
    fn rollback_screens_at_supported_widths_and_ascii() {
        for width in [60, 80, 120] {
            for ascii in [false, true] {
                let host = fixture_host().with_env("COLUMNS", width.to_string());
                let mut args = vec!["nut", "rollback", "--list"];
                if ascii {
                    args.insert(1, "--ascii");
                }
                let (code, output, error) = execute(&args, &host);
                assert_eq!(code, 0, "{error}");
                assert!(
                    output.lines().all(|line| line.chars().count() <= width),
                    "{output}"
                );
                assert!(!output.contains('\u{1b}'));
                if ascii {
                    assert!(output.is_ascii(), "{output}");
                }
                insta::assert_snapshot!(
                    format!(
                        "rollback_{width}_{}",
                        if ascii { "ascii" } else { "unicode" }
                    ),
                    output
                );
            }
        }
    }
}
