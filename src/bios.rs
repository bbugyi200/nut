use std::ffi::OsString;
use std::io::Write;
use std::path::Path;

use serde::Serialize;

use crate::cli::{BiosAction, ColorChoice, ProfileAction};
use crate::config::{ConfigError, HostConfig, load_config};
use crate::error::NutError;
use crate::firmware::audit::{BootCheck, CheckStatus, audit_boot};
use crate::firmware::backend::SettingReading;
use crate::firmware::{
    AttestationAnswer, AttestationFreshness, AttestationReceipt, DmiInfo, PendingMarker,
    backend_for, clear_pending, efivar_fingerprint, firmware_setup_supported, freshness,
    load_board_catalog, load_pending, load_receipt, pending_path, receipt_path, resolve_profile,
    save_json, unix_secs,
};
use crate::host::Host;
use crate::profiles::{BoardProfile, ProfileCatalog, ProfileId};
use crate::ui::glyphs::Glyphs;
use crate::ui::theme::{Theme, Tone};
use crate::ui::{UiContext, UiOptions};

pub struct BiosRun<'a> {
    pub action: Option<BiosAction>,
    pub json: bool,
    pub ascii: bool,
    pub color: ColorChoice,
    pub verbose: bool,
    pub config: Option<&'a Path>,
}

pub fn run<H: Host, W: Write>(
    invocation: BiosRun<'_>,
    host: &H,
    stdout: &mut W,
) -> Result<(), NutError> {
    let ui = UiContext::new(
        UiOptions {
            color: invocation.color,
            ascii: invocation.ascii,
        },
        host,
        host.stdout_is_terminal() && !invocation.json,
    );
    match invocation.action {
        None => show(
            invocation.json,
            invocation.verbose,
            invocation.config,
            host,
            &ui,
            stdout,
        ),
        Some(BiosAction::Guide) => guide(invocation.json, invocation.config, host, &ui, stdout),
        Some(BiosAction::Setup { yes }) => setup(yes, invocation.config, host, &ui, stdout),
        Some(BiosAction::Attest) => attest(invocation.config, host, &ui, stdout),
        Some(BiosAction::Profile { action }) => {
            profile_command(action, invocation.json, host, &ui, stdout)
        }
    }
}

pub fn write_pending_reminder<H: Host, W: Write>(
    host: &H,
    ascii: bool,
    stdout: &mut W,
) -> Result<bool, NutError> {
    if load_pending(host)?.is_none() {
        return Ok(false);
    }
    let glyphs = Glyphs::new(ascii, host.env("TERM").as_deref(), None);
    writeln!(
        stdout,
        " {} BIOS attestation pending  {} sudo nut bios attest",
        glyphs.pending, glyphs.arrow
    )
    .map_err(write_error)?;
    Ok(true)
}

fn load_snapshot<H: Host>(
    host: &H,
    config_override: Option<&Path>,
    catalog: &ProfileCatalog,
) -> Result<(DmiInfo, Option<HostConfig>, BoardProfile), NutError> {
    let dmi = DmiInfo::read(host)?;
    let config = optional_config(host, config_override)?;
    let profile = resolve_profile(catalog, config.as_ref(), &dmi).clone();
    Ok((dmi, config, profile))
}

fn optional_config<H: Host>(
    host: &H,
    config_override: Option<&Path>,
) -> Result<Option<HostConfig>, NutError> {
    match load_config(host, config_override) {
        Ok((_, _, config)) => Ok(Some(config)),
        Err(ConfigError::NotFound(_)) if config_override.is_none() => Ok(None),
        Err(ConfigError::NoHome) if config_override.is_none() => Ok(None),
        Err(error) => Err(NutError::operational(error.to_string())),
    }
}

fn show<H: Host, W: Write>(
    json: bool,
    verbose: bool,
    config_override: Option<&Path>,
    host: &H,
    ui: &UiContext,
    stdout: &mut W,
) -> Result<(), NutError> {
    let catalog =
        load_board_catalog(host).map_err(|error| NutError::operational(error.to_string()))?;
    let (dmi, config, profile) = load_snapshot(host, config_override, &catalog)?;
    let report = collect_report(host, &dmi, config.as_ref(), &profile, verbose)?;
    if json {
        return write_json(stdout, &report);
    }
    writeln!(
        stdout,
        " {}",
        Theme::paint(
            Tone::Info,
            &format!(
                "nut bios · {} · BIOS {}",
                profile.display_name(),
                dmi.bios_version.as_deref().unwrap_or("unknown")
            ),
            ui.color_enabled
        )
    )
    .map_err(write_error)?;
    writeln!(
        stdout,
        " {:<12} {}  {}",
        "Board", dmi.board_vendor, dmi.board_name
    )
    .map_err(write_error)?;
    writeln!(
        stdout,
        " {:<12} {}  backend {}",
        "Profile",
        profile.id,
        backend_name(profile.backend)
    )
    .map_err(write_error)?;
    if let Some(fingerprint) = &report.fingerprint {
        let shown = if fingerprint.len() > 16 {
            format!("{}…", &fingerprint[..16])
        } else {
            fingerprint.clone()
        };
        writeln!(stdout, " {:<12} {shown}", "Fingerprint").map_err(write_error)?;
    }
    writeln!(stdout).map_err(write_error)?;
    writeln!(
        stdout,
        " {}",
        Theme::paint(Tone::Info, "Settings", ui.color_enabled)
    )
    .map_err(write_error)?;
    for setting in &report.settings {
        let glyph = setting_glyph(&setting.state, &ui.glyphs);
        writeln!(
            stdout,
            " {glyph} {:<14} {} = {}   {}",
            setting.id, setting.label, setting.required, setting.state
        )
        .map_err(write_error)?;
        writeln!(
            stdout,
            "                   {}",
            setting.menu_path.join(" → ")
        )
        .map_err(write_error)?;
        if setting.state != "attested" && setting.state != "verified" {
            writeln!(
                stdout,
                "                   {} sudo nut bios attest",
                ui.glyphs.arrow
            )
            .map_err(write_error)?;
        }
    }
    writeln!(stdout).map_err(write_error)?;
    writeln!(
        stdout,
        " {}",
        Theme::paint(Tone::Info, "Boot readiness", ui.color_enabled)
    )
    .map_err(write_error)?;
    for check in &report.boot {
        let glyph = check_glyph(check.status, &ui.glyphs);
        writeln!(stdout, " {glyph} {:<16} {}", check.id, check.evidence).map_err(write_error)?;
        if let Some(fix) = &check.fix {
            writeln!(stdout, "                   {} {fix}", ui.glyphs.arrow)
                .map_err(write_error)?;
        }
    }
    writeln!(stdout).map_err(write_error)?;
    writeln!(
        stdout,
        " {}",
        Theme::paint(Tone::Info, "Attestation", ui.color_enabled)
    )
    .map_err(write_error)?;
    let glyph = match report.attestation.status.as_str() {
        "fresh" => ui.glyphs.ok,
        "stale" | "pending" => ui.glyphs.warn,
        _ => ui.glyphs.pending,
    };
    writeln!(stdout, " {glyph} {}", report.attestation.summary).map_err(write_error)?;
    if let Some(fix) = &report.attestation.fix {
        writeln!(stdout, "   {} {fix}", ui.glyphs.arrow).map_err(write_error)?;
    }
    Ok(())
}

fn guide<H: Host, W: Write>(
    json: bool,
    config_override: Option<&Path>,
    host: &H,
    ui: &UiContext,
    stdout: &mut W,
) -> Result<(), NutError> {
    let catalog =
        load_board_catalog(host).map_err(|error| NutError::operational(error.to_string()))?;
    let (dmi, _config, profile) = load_snapshot(host, config_override, &catalog)?;
    let steps = guide_steps(&profile, firmware_setup_supported(host)?);
    if json {
        return write_json(
            stdout,
            &serde_json::json!({
                "schema_version": 1,
                "profile": profile.id.as_str(),
                "board_vendor": dmi.board_vendor,
                "board_name": dmi.board_name,
                "steps": steps,
                "caveats": profile.caveats,
            }),
        );
    }
    writeln!(
        stdout,
        " {}",
        Theme::paint(
            Tone::Info,
            &format!("nut bios guide · {}", profile.id),
            ui.color_enabled
        )
    )
    .map_err(write_error)?;
    writeln!(stdout, " {} {}", dmi.board_vendor, dmi.board_name).map_err(write_error)?;
    writeln!(stdout).map_err(write_error)?;
    for (index, step) in steps.iter().enumerate() {
        writeln!(stdout, " {}. {}", index + 1, step.title).map_err(write_error)?;
        for line in &step.detail {
            writeln!(stdout, "    {line}").map_err(write_error)?;
        }
    }
    if !profile.caveats.is_empty() {
        writeln!(stdout).map_err(write_error)?;
        writeln!(
            stdout,
            " {}",
            Theme::paint(Tone::Info, "Caveats", ui.color_enabled)
        )
        .map_err(write_error)?;
        for caveat in &profile.caveats {
            writeln!(stdout, "  {} {caveat}", ui.glyphs.bullet).map_err(write_error)?;
        }
    }
    Ok(())
}

fn setup<H: Host, W: Write>(
    yes: bool,
    config_override: Option<&Path>,
    host: &H,
    ui: &UiContext,
    stdout: &mut W,
) -> Result<(), NutError> {
    if yes {
        return Err(NutError::operational(
            "nut bios setup refuses --yes; type the hostname at the prompt",
        ));
    }
    if !host.stdin_is_terminal() || !host.stdout_is_terminal() {
        return Err(NutError::operational(
            "nut bios setup needs a TTY on stdin and stdout",
        ));
    }
    if host.euid() != 0 {
        return Err(NutError::operational(
            "nut bios setup needs root → sudo nut bios setup",
        ));
    }
    let hostname = host.hostname()?;
    let typed = host.prompt_line(&format!("Type {hostname} to continue: "))?;
    if typed.trim() != hostname {
        return Err(NutError::operational(
            "hostname did not match; BIOS setup cancelled",
        ));
    }
    let catalog =
        load_board_catalog(host).map_err(|error| NutError::operational(error.to_string()))?;
    let (dmi, _config, profile) = load_snapshot(host, config_override, &catalog)?;
    writeln!(
        stdout,
        " {}",
        Theme::paint(Tone::Info, "BIOS cheat card", ui.color_enabled)
    )
    .map_err(write_error)?;
    for (index, step) in guide_steps(&profile, false).iter().enumerate() {
        writeln!(stdout, " {}. {}", index + 1, step.title).map_err(write_error)?;
    }
    let pending = PendingMarker {
        schema_version: 1,
        profile_id: profile.id.to_string(),
        bios_version: dmi.bios_version.clone(),
        recorded_unix: unix_secs(host.now()),
    };
    save_json(host, &pending_path(), &pending)?;
    writeln!(
        stdout,
        " {} pending attestation recorded  {} sudo nut bios attest after saving",
        ui.glyphs.ok, ui.glyphs.arrow
    )
    .map_err(write_error)?;
    if firmware_setup_supported(host)? {
        writeln!(stdout, " rebooting into firmware setup").map_err(write_error)?;
        let output = host.run_program(
            "systemctl".as_ref(),
            &[OsString::from("reboot"), OsString::from("--firmware-setup")],
        )?;
        if output.status != Some(0) {
            return Err(NutError::operational(format!(
                "systemctl reboot --firmware-setup failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
    } else {
        writeln!(
            stdout,
            " firmware setup reboot is not advertised; press {} at POST",
            profile.keys.setup
        )
        .map_err(write_error)?;
    }
    Ok(())
}

fn attest<H: Host, W: Write>(
    config_override: Option<&Path>,
    host: &H,
    ui: &UiContext,
    stdout: &mut W,
) -> Result<(), NutError> {
    if !host.stdin_is_terminal() || !host.stdout_is_terminal() {
        return Err(NutError::operational(
            "nut bios attest needs a TTY on stdin and stdout",
        ));
    }
    if host.euid() != 0 {
        return Err(NutError::operational(
            "nut bios attest needs root → sudo nut bios attest",
        ));
    }
    let catalog =
        load_board_catalog(host).map_err(|error| NutError::operational(error.to_string()))?;
    let (dmi, _config, profile) = load_snapshot(host, config_override, &catalog)?;
    let fingerprint = efivar_fingerprint(host, &profile.fingerprint_efivars)?
        .unwrap_or_else(|| "none".to_owned());
    let mut answers = Vec::new();
    for setting in &profile.settings {
        let path = setting.menu_path.join(" → ");
        let prompt = format!("Is {path} set to {}? [y/N] ", setting.required);
        let answer = host.prompt_line(&prompt)?;
        let confirmed = matches!(answer.trim(), "y" | "Y" | "yes" | "YES");
        answers.push(AttestationAnswer {
            id: setting.id.clone(),
            confirmed,
        });
    }
    let receipt = AttestationReceipt {
        schema_version: 1,
        profile_id: profile.id.to_string(),
        board_vendor: dmi.board_vendor,
        board_name: dmi.board_name,
        bios_version: dmi.bios_version,
        bios_date: dmi.bios_date,
        fingerprint,
        answers,
        recorded_unix: unix_secs(host.now()),
    };
    save_json(host, &receipt_path(), &receipt)?;
    clear_pending(host)?;
    writeln!(
        stdout,
        " {} attestation recorded for BIOS {}  {} {}",
        ui.glyphs.ok,
        receipt.bios_version.as_deref().unwrap_or("unknown"),
        ui.glyphs.arrow,
        receipt_path().display()
    )
    .map_err(write_error)?;
    Ok(())
}

fn profile_command<H: Host, W: Write>(
    action: Option<ProfileAction>,
    json: bool,
    host: &H,
    ui: &UiContext,
    stdout: &mut W,
) -> Result<(), NutError> {
    let catalog =
        load_board_catalog(host).map_err(|error| NutError::operational(error.to_string()))?;
    match action.unwrap_or(ProfileAction::List) {
        ProfileAction::List => profile_list(json, host, &catalog, ui, stdout),
        ProfileAction::Show { id } => profile_show(json, &id, &catalog, stdout),
        ProfileAction::New => profile_new(host, stdout),
    }
}

fn profile_list<H: Host, W: Write>(
    json: bool,
    host: &H,
    catalog: &ProfileCatalog,
    ui: &UiContext,
    stdout: &mut W,
) -> Result<(), NutError> {
    let dmi = DmiInfo::read(host)?;
    let matched = catalog
        .match_board(&dmi.board_vendor, &dmi.board_name)
        .map(|profile| profile.id.as_str().to_owned());
    let profiles: Vec<_> = catalog
        .board_profiles()
        .map(|profile| {
            serde_json::json!({
                "id": profile.id.as_str(),
                "backend": backend_name(profile.backend),
                "matched": matched.as_deref() == Some(profile.id.as_str()),
                "display_name": profile.display_name(),
            })
        })
        .collect();
    if json {
        return write_json(
            stdout,
            &serde_json::json!({
                "schema_version": 1,
                "board_vendor": dmi.board_vendor,
                "board_name": dmi.board_name,
                "profiles": profiles,
            }),
        );
    }
    writeln!(
        stdout,
        " {}",
        Theme::paint(Tone::Info, "Board profiles", ui.color_enabled)
    )
    .map_err(write_error)?;
    for profile in catalog.board_profiles() {
        let mark = if matched.as_deref() == Some(profile.id.as_str()) {
            "*"
        } else {
            " "
        };
        writeln!(
            stdout,
            " {mark} {:<24} {:<22} {}",
            profile.id,
            backend_name(profile.backend),
            profile.display_name()
        )
        .map_err(write_error)?;
    }
    Ok(())
}

fn profile_show<W: Write>(
    json: bool,
    id: &str,
    catalog: &ProfileCatalog,
    stdout: &mut W,
) -> Result<(), NutError> {
    let profile_id = ProfileId::new(id).map_err(|error| NutError::usage(error.to_string()))?;
    let Some(profile) = catalog.board(&profile_id) else {
        return Err(NutError::operational(format!("unknown board profile {id}")));
    };
    if json {
        return write_json(stdout, profile);
    }
    writeln!(stdout, "id              {}", profile.id).map_err(write_error)?;
    writeln!(stdout, "backend        {}", backend_name(profile.backend)).map_err(write_error)?;
    if let Some(spec) = &profile.match_ {
        writeln!(
            stdout,
            "match           {} / {}",
            spec.board_vendor,
            spec.board_name.join(", ")
        )
        .map_err(write_error)?;
    }
    writeln!(
        stdout,
        "keys            setup {}  save {}",
        profile.keys.setup, profile.keys.save
    )
    .map_err(write_error)?;
    for setting in &profile.settings {
        let optional = if setting.optional { " (optional)" } else { "" };
        writeln!(
            stdout,
            "setting         {} = {}{}",
            setting.menu_path.join(" → "),
            setting.required,
            optional
        )
        .map_err(write_error)?;
        writeln!(stdout, "                {}", setting.why).map_err(write_error)?;
    }
    for caveat in &profile.caveats {
        writeln!(stdout, "caveat          {caveat}").map_err(write_error)?;
    }
    Ok(())
}

fn profile_new<H: Host, W: Write>(host: &H, stdout: &mut W) -> Result<(), NutError> {
    let dmi = DmiInfo::read(host)?;
    writeln!(
        stdout,
        r#"schema = 1
id = "todo-board-id"
backend = "manual"
fingerprint_efivars = [
    "Setup-*",
]
caveats = [
    "TODO: vendor-specific AC-loss caveats.",
]
sources = [
    "TODO: board manual section",
]

[match]
board_vendor = "{}"
board_name = "{}"

[keys]
setup = "Del"
advanced_mode = "F7"
save = "F10"

[[settings]]
id = "ac-power-loss"
menu_path = ["TODO", "Restore On AC Power Loss"]
required = "Power On"
why = "TODO: why this board must power on after AC returns."
"#,
        dmi.board_vendor.replace('"', "\\\""),
        dmi.board_name.replace('"', "\\\"")
    )
    .map_err(write_error)?;
    Ok(())
}

#[derive(Serialize)]
struct BiosReport {
    schema_version: u32,
    board_vendor: String,
    board_name: String,
    bios_version: Option<String>,
    bios_date: Option<String>,
    profile: String,
    backend: String,
    fingerprint: Option<String>,
    firmware_setup_supported: bool,
    attestation: AttestationJson,
    settings: Vec<SettingJson>,
    boot: Vec<BootCheck>,
}

#[derive(Serialize)]
struct AttestationJson {
    status: String,
    summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fix: Option<String>,
}

#[derive(Serialize)]
struct SettingJson {
    id: String,
    label: String,
    menu_path: Vec<String>,
    required: String,
    optional: bool,
    state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    current: Option<String>,
    why: String,
}

#[derive(Serialize)]
struct GuideStep {
    title: String,
    detail: Vec<String>,
}

fn collect_report<H: Host>(
    host: &H,
    dmi: &DmiInfo,
    config: Option<&HostConfig>,
    profile: &BoardProfile,
    _verbose: bool,
) -> Result<BiosReport, NutError> {
    let fingerprint = efivar_fingerprint(host, &profile.fingerprint_efivars)?;
    let receipt = load_receipt(host)?;
    let pending = load_pending(host)?;
    let freshness = freshness(
        pending.as_ref(),
        receipt.as_ref(),
        dmi,
        fingerprint.as_deref(),
    );
    let backend = backend_for(profile);
    let readings = backend.read_settings(host, profile, receipt.as_ref(), &freshness)?;
    let attestation = attestation_json(&freshness);
    Ok(BiosReport {
        schema_version: 1,
        board_vendor: dmi.board_vendor.clone(),
        board_name: dmi.board_name.clone(),
        bios_version: dmi.bios_version.clone(),
        bios_date: dmi.bios_date.clone(),
        profile: profile.id.to_string(),
        backend: backend_name(profile.backend).to_owned(),
        fingerprint,
        firmware_setup_supported: firmware_setup_supported(host)?,
        attestation,
        settings: readings.into_iter().map(setting_json).collect(),
        boot: audit_boot(host, config),
    })
}

fn attestation_json(freshness: &AttestationFreshness) -> AttestationJson {
    match freshness {
        AttestationFreshness::Missing => AttestationJson {
            status: "missing".into(),
            summary: "never attested".into(),
            reason: None,
            fix: Some("sudo nut bios attest".into()),
        },
        AttestationFreshness::Pending => AttestationJson {
            status: "pending".into(),
            summary: "attestation pending after BIOS setup".into(),
            reason: None,
            fix: Some("sudo nut bios attest".into()),
        },
        AttestationFreshness::Fresh => AttestationJson {
            status: "fresh".into(),
            summary: "attestation matches this BIOS version and efivar fingerprint".into(),
            reason: None,
            fix: None,
        },
        AttestationFreshness::Stale { reason } => AttestationJson {
            status: "stale".into(),
            summary: format!("{reason}; re-attest"),
            reason: Some(reason.clone()),
            fix: Some("sudo nut bios attest".into()),
        },
    }
}

fn setting_json(reading: SettingReading) -> SettingJson {
    SettingJson {
        id: reading.id.clone(),
        label: reading.label().to_owned(),
        menu_path: reading.menu_path,
        required: reading.required,
        optional: reading.optional,
        state: reading.state.as_str().to_owned(),
        current: reading.current,
        why: reading.why,
    }
}

fn guide_steps(profile: &BoardProfile, firmware_setup: bool) -> Vec<GuideStep> {
    let mut steps = Vec::new();
    if firmware_setup {
        steps.push(GuideStep {
            title: format!(
                "Reboot into firmware setup (sudo nut bios setup) or press {} at POST",
                profile.keys.setup
            ),
            detail: vec!["A keyboard and monitor at the machine are required.".into()],
        });
    } else {
        steps.push(GuideStep {
            title: format!("Reboot and press {} at POST", profile.keys.setup),
            detail: vec!["or run: sudo nut bios setup".into()],
        });
    }
    if let Some(advanced) = &profile.keys.advanced_mode {
        steps.push(GuideStep {
            title: format!("Press {advanced} for Advanced Mode"),
            detail: Vec::new(),
        });
    }
    for setting in &profile.settings {
        let optional = if setting.optional { " (optional)" } else { "" };
        steps.push(GuideStep {
            title: format!("{}{optional}", setting.menu_path.join(" → ")),
            detail: vec![format!("set to {}", setting.required), setting.why.clone()],
        });
    }
    steps.push(GuideStep {
        title: format!("Press {} to save and reboot", profile.keys.save),
        detail: vec!["Then run: sudo nut bios attest".into()],
    });
    steps
}

fn backend_name(kind: crate::profiles::BiosBackendKind) -> &'static str {
    match kind {
        crate::profiles::BiosBackendKind::Manual => "manual",
        crate::profiles::BiosBackendKind::FirmwareAttributes => "firmware-attributes",
    }
}

fn setting_glyph(state: &str, glyphs: &Glyphs) -> &'static str {
    match state {
        "attested" | "verified" => glyphs.ok,
        "stale" | "mismatch" => glyphs.warn,
        _ => glyphs.pending,
    }
}

fn check_glyph(status: CheckStatus, glyphs: &Glyphs) -> &'static str {
    match status {
        CheckStatus::Pass => glyphs.ok,
        CheckStatus::Warn => glyphs.warn,
        CheckStatus::Fail => glyphs.fail,
        CheckStatus::Unknown => glyphs.pending,
    }
}

fn write_json<W: Write, T: Serialize>(stdout: &mut W, value: &T) -> Result<(), NutError> {
    serde_json::to_writer_pretty(&mut *stdout, value)
        .map_err(|error| NutError::operational(format!("cannot write JSON output: {error}")))?;
    writeln!(stdout).map_err(write_error)?;
    Ok(())
}

fn write_error(error: std::io::Error) -> NutError {
    NutError::operational(format!("cannot write output: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::firmware::{AttestationReceipt, BIOS_STATE_DIR};
    use crate::host::{FakeHost, Host, ProcessOutput};
    use crate::run_with_host;

    fn athena_host() -> FakeHost {
        FakeHost::default()
            .with_hostname("athena")
            .with_file("/sys/class/dmi/id/board_vendor", b"ASUSTeK COMPUTER INC.\n")
            .with_file("/sys/class/dmi/id/board_name", b"PRIME TRX40-PRO\n")
            .with_file("/sys/class/dmi/id/bios_version", b"1101\n")
            .with_file("/sys/class/dmi/id/bios_date", b"07/01/2020\n")
            .with_file(
                "/sys/firmware/efi/efivars/Setup-ec87d643-eba4-4bb5-a1e5-3f3e36b20da9",
                b"\x07\x00\x00\x00setup",
            )
            .with_file(
                "/sys/firmware/efi/efivars/AmdSetup-3a997502-647a-4c82-998e-52ef9486a247",
                b"\x07\x00\x00\x00amd",
            )
            .with_program(
                "efibootmgr",
                ProcessOutput {
                    status: Some(0),
                    stdout:
                        b"BootOrder: 0002,0001\nBoot0002* debian\nBoot0001* Windows Boot Manager\n"
                            .to_vec(),
                    stderr: Vec::new(),
                },
            )
            .with_program_args(
                "findmnt",
                ["--verify"],
                ProcessOutput {
                    status: Some(0),
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                },
            )
            .with_dir("/var/log/journal")
            .with_file("/proc/sys/kernel/panic", b"10\n")
    }

    #[test]
    fn bios_matches_trx40_and_renders_unknown_settings() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_host(
            ["nut", "--ascii", "bios"],
            &athena_host(),
            &mut out,
            &mut err,
        );
        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("asus-prime-trx40-pro"));
        assert!(text.contains("PRIME TRX40-PRO"));
        assert!(text.contains("Restore On AC Power Loss"));
        assert!(text.contains("unknown"));
        assert!(text.contains("boot.efi-order"));
        assert!(err.is_empty());
    }

    #[test]
    fn bios_guide_is_a_numbered_keyboard_walkthrough() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_host(
            ["nut", "--ascii", "bios", "guide"],
            &athena_host(),
            &mut out,
            &mut err,
        );
        assert_eq!(code, 0);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("1. "));
        assert!(text.contains("press Del"));
        assert!(text.contains("F7"));
        assert!(text.contains("Power On"));
        assert!(text.contains("30 s"));
        assert!(err.is_empty());
    }

    #[test]
    fn setup_refuses_yes_and_missing_tty() {
        let host = athena_host().with_euid(0);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_host(["nut", "bios", "setup", "--yes"], &host, &mut out, &mut err);
        assert_eq!(code, 1);
        assert!(String::from_utf8(err).unwrap().contains("refuses --yes"));
    }

    #[test]
    fn setup_records_pending_and_falls_back_to_del() {
        let host = athena_host()
            .with_euid(0)
            .with_stdin_tty(true)
            .with_tty(true, true)
            .with_prompt_lines(["athena"]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_host(
            ["nut", "--ascii", "bios", "setup"],
            &host,
            &mut out,
            &mut err,
        );
        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("press Del at POST"));
        assert!(text.contains("pending attestation"));
        assert!(
            host.read_file(Path::new("/var/lib/nut-cli/bios/pending.json"))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn attest_writes_a_receipt_pinned_to_version_and_fingerprint() {
        let host = athena_host()
            .with_euid(0)
            .with_stdin_tty(true)
            .with_tty(true, true)
            .with_prompt_lines(["y", "y", "y"]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_host(["nut", "bios", "attest"], &host, &mut out, &mut err);
        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
        let bytes = host
            .read_file(Path::new("/var/lib/nut-cli/bios/attestation.json"))
            .unwrap()
            .unwrap();
        let receipt: AttestationReceipt = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(receipt.bios_version.as_deref(), Some("1101"));
        assert_eq!(receipt.fingerprint.len(), 64);
        assert!(receipt.answers.iter().all(|answer| answer.confirmed));
        let _ = BIOS_STATE_DIR;
    }

    #[test]
    fn profile_new_scaffolds_live_dmi_with_todos() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with_host(
            ["nut", "bios", "profile", "new"],
            &athena_host(),
            &mut out,
            &mut err,
        );
        assert_eq!(code, 0);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("ASUSTeK COMPUTER INC."));
        assert!(text.contains("PRIME TRX40-PRO"));
        assert!(text.contains("TODO"));
    }

    #[test]
    fn status_prints_pending_reminder_and_reads_the_ups() {
        use crate::fake_upsd::{FakeUpsdFixture, serve_client};
        use std::collections::BTreeMap;
        use std::net::TcpListener;
        use std::thread;

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let fixture = FakeUpsdFixture {
            ups_name: "primary".into(),
            description: "Reminder test".into(),
            variables: BTreeMap::from([("ups.status".into(), "OL".into())]),
        };
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            serve_client(stream, &fixture).unwrap();
        });
        let host = athena_host().with_file(
            "/var/lib/nut-cli/bios/pending.json",
            br#"{"schema_version":1,"profile_id":"asus-prime-trx40-pro","bios_version":"1101","recorded_unix":1}"#,
        );
        let mut out = Vec::new();
        let mut err = Vec::new();
        let args = [
            "nut".to_owned(),
            "--ascii".into(),
            "status".into(),
            "--upsd-address".into(),
            "127.0.0.1".into(),
            "--upsd-port".into(),
            port.to_string(),
        ];
        let code = run_with_host(args, &host, &mut out, &mut err);
        server.join().unwrap();
        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
        let output = String::from_utf8(out).unwrap();
        assert!(output.contains("BIOS attestation pending"));
        assert!(output.contains("nut status · primary"));
        assert!(err.is_empty());
    }
}
