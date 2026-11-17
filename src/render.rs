use std::path::Path;

use serde_json::{Value, json};
use similar::{ChangeTag, TextDiff};
use thiserror::Error;

use crate::config::ResolvedConfig;
use crate::error::NutError;
use crate::host::Host;

pub const MANAGED_HEADER: &str = "# Managed by nut. Edit the host config instead of this file.";
pub const HOOK_NAME: &str = "nut-upssched-cmd";
pub const HOOK_PATH: &str = "/usr/local/sbin/nut-upssched-cmd";
pub const JOURNAL_TAG: &str = "nut-upssched";
pub const OPERATOR_USER: &str = "nut-operator";
pub const UPSMON_PLACEHOLDER: &str = "@UPSMON_PASSWORD@";
pub const OPERATOR_PLACEHOLDER: &str = "@OPERATOR_PASSWORD@";

pub const LIVE_PATHS: &[(&str, &str)] = &[
    ("nut.conf", "/etc/nut/nut.conf"),
    ("ups.conf", "/etc/nut/ups.conf"),
    ("upsd.users", "/etc/nut/upsd.users"),
    ("upsmon.conf", "/etc/nut/upsmon.conf"),
    ("upssched.conf", "/etc/nut/upssched.conf"),
    (HOOK_NAME, HOOK_PATH),
];

pub const OPERATOR_INSTCMDS: &[&str] = &[
    "beeper.disable",
    "beeper.enable",
    "beeper.mute",
    "test.battery.start.quick",
    "test.battery.stop",
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderedFiles {
    pub nut_conf: String,
    pub ups_conf: String,
    pub upsd_users: String,
    pub upsmon_conf: String,
    pub upssched_conf: String,
    pub hook: String,
}

impl RenderedFiles {
    pub fn iter(&self) -> [(&'static str, &str); 6] {
        [
            ("nut.conf", self.nut_conf.as_str()),
            ("ups.conf", self.ups_conf.as_str()),
            ("upsd.users", self.upsd_users.as_str()),
            ("upsmon.conf", self.upsmon_conf.as_str()),
            ("upssched.conf", self.upssched_conf.as_str()),
            (HOOK_NAME, self.hook.as_str()),
        ]
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.iter()
            .into_iter()
            .find_map(|(file, contents)| (file == name).then_some(contents))
    }

    pub fn write_to(&self, dir: &Path) -> Result<(), std::io::Error> {
        std::fs::create_dir_all(dir)?;
        for (name, contents) in self.iter() {
            std::fs::write(dir.join(name), contents)?;
        }
        Ok(())
    }

    pub fn json_value(&self) -> Value {
        json!({
            "schema_version": 1,
            "files": {
                "nut.conf": self.nut_conf,
                "ups.conf": self.ups_conf,
                "upsd.users": self.upsd_users,
                "upsmon.conf": self.upsmon_conf,
                "upssched.conf": self.upssched_conf,
                HOOK_NAME: self.hook,
            }
        })
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum RenderError {
    #[error(
        "refusing to interpolate {what} {value:?}: values may not contain quotes, newlines, or shell/NUT metacharacters"
    )]
    UnsafeValue { what: &'static str, value: String },
    #[error("invalid extra ups.conf value for {0}: {1:?}")]
    UnsafeExtra(String, String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiffStatus {
    Identical,
    Differs { diff: String },
    Missing,
    NeedsRoot,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileDiff {
    pub name: &'static str,
    pub live_path: &'static str,
    pub status: DiffStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiffReport {
    pub files: Vec<FileDiff>,
}

impl DiffReport {
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        for file in &self.files {
            match &file.status {
                DiffStatus::Identical => {
                    out.push_str(&format!("{}  identical\n", file.live_path));
                }
                DiffStatus::Missing => {
                    out.push_str(&format!("{}  missing\n", file.live_path));
                }
                DiffStatus::NeedsRoot => {
                    out.push_str(&format!("{}  contents need root\n", file.live_path));
                }
                DiffStatus::Differs { diff } => {
                    out.push_str(&format!("{}  differs\n{diff}", file.live_path));
                    if !diff.ends_with('\n') {
                        out.push('\n');
                    }
                }
            }
        }
        out
    }

    pub fn json_value(&self) -> Value {
        json!({
            "schema_version": 1,
            "files": self.files.iter().map(|file| {
                json!({
                    "name": file.name,
                    "path": file.live_path,
                    "status": match &file.status {
                        DiffStatus::Identical => "identical",
                        DiffStatus::Missing => "missing",
                        DiffStatus::NeedsRoot => "needs_root",
                        DiffStatus::Differs { .. } => "differs",
                    },
                    "diff": match &file.status {
                        DiffStatus::Differs { diff } => Value::String(diff.clone()),
                        _ => Value::Null,
                    }
                })
            }).collect::<Vec<_>>(),
        })
    }
}

pub fn render(config: &ResolvedConfig) -> Result<RenderedFiles, RenderError> {
    Ok(RenderedFiles {
        nut_conf: render_nut_conf(config)?,
        ups_conf: render_ups_conf(config)?,
        upsd_users: render_upsd_users()?,
        upsmon_conf: render_upsmon_conf(config)?,
        upssched_conf: render_upssched_conf(config)?,
        hook: render_hook(config)?,
    })
}

pub fn render_nut_conf(config: &ResolvedConfig) -> Result<String, RenderError> {
    let wait = config.shutdown.poweroff_wait.compact();
    validate_token("POWEROFF_WAIT", &wait)?;
    Ok(format!(
        "{MANAGED_HEADER}\nMODE=standalone\nPOWEROFF_WAIT={wait}\n"
    ))
}

pub fn render_ups_conf(config: &ResolvedConfig) -> Result<String, RenderError> {
    let name = validate_ident("UPS name", &config.ups_name)?;
    let driver = validate_ident("driver", &config.driver.value)?;
    let port = validate_token("port", &config.port.value)?;
    let desc = validate_description(&config.description.value)?;
    let offdelay = config.offdelay.value.as_secs();
    let ondelay = config.ondelay.value.as_secs();
    let mut global = String::new();
    let maxretry = config
        .extra
        .get("maxretry")
        .cloned()
        .or_else(|| default_maxretry(&config.driver.value));
    let retrydelay = config
        .extra
        .get("retrydelay")
        .cloned()
        .or_else(|| default_retrydelay(&config.driver.value));
    if let Some(value) = maxretry {
        let value = validate_token("maxretry", &value)?;
        global.push_str(&format!("maxretry = {value}\n"));
    }
    if let Some(value) = retrydelay {
        let value = validate_token("retrydelay", &value)?;
        global.push_str(&format!("retrydelay = {value}\n"));
    }
    let mut stanza = format!("[{name}]\n    driver = {driver}\n    port = {port}\n");
    let dummy = config.driver.value == "dummy-ups";
    if !dummy {
        let vendorid = validate_ident("vendorid", &config.vendorid.value)?;
        let productid = validate_ident("productid", &config.productid.value)?;
        stanza.push_str(&format!(
            "    vendorid = {vendorid}\n    productid = {productid}\n"
        ));
    }
    stanza.push_str(&format!("    desc = \"{desc}\"\n"));
    // offdelay/ondelay are hardware driver variables. dummy-ups rejects them
    // at startup ("'offdelay' is not a valid variable name for this driver");
    // its delays come from the ups.delay.* values in the device file.
    if !dummy {
        stanza.push_str(&format!(
            "    offdelay = {offdelay}\n    ondelay = {ondelay}\n"
        ));
    }
    for (key, value) in &config.extra {
        if key == "maxretry" || key == "retrydelay" {
            continue;
        }
        let key = validate_ident("extra key", key)
            .map_err(|_| RenderError::UnsafeExtra(key.clone(), value.clone()))?;
        let value = validate_token("extra value", value)
            .map_err(|_| RenderError::UnsafeExtra(key.to_owned(), value.clone()))?;
        stanza.push_str(&format!("    {key} = {value}\n"));
    }
    if let Some(serial) = &config.ups.usb_match.serial {
        let serial = validate_token("serial", serial)?;
        stanza.push_str(&format!("    serial = {serial}\n"));
    }
    if global.is_empty() {
        Ok(format!("{MANAGED_HEADER}\n{stanza}"))
    } else {
        Ok(format!("{MANAGED_HEADER}\n{global}\n{stanza}"))
    }
}

pub fn render_upsd_users() -> Result<String, RenderError> {
    validate_token("upsmon password placeholder", UPSMON_PLACEHOLDER)?;
    validate_token("operator password placeholder", OPERATOR_PLACEHOLDER)?;
    let mut out = format!(
        "{MANAGED_HEADER}\n\
[upsmon]\n\
    password = {UPSMON_PLACEHOLDER}\n\
    upsmon primary\n\
\n\
[{OPERATOR_USER}]\n\
    password = {OPERATOR_PLACEHOLDER}\n"
    );
    for cmd in OPERATOR_INSTCMDS {
        out.push_str(&format!("    instcmds = {cmd}\n"));
    }
    Ok(out)
}

pub fn render_upsmon_conf(config: &ResolvedConfig) -> Result<String, RenderError> {
    let name = validate_ident("UPS name", &config.ups_name)?;
    Ok(format!(
        "{MANAGED_HEADER}\n\
RUN_AS_USER nut\n\
MONITOR {name}@localhost 1 upsmon {UPSMON_PLACEHOLDER} primary\n\
MINSUPPLIES 1\n\
SHUTDOWNCMD \"/sbin/shutdown -h +0\"\n\
POLLFREQ 5\n\
POLLFREQALERT 5\n\
HOSTSYNC 15\n\
DEADTIME 15\n\
POWERDOWNFLAG \"/etc/killpower\"\n\
OFFDURATION 30\n\
RBWARNTIME 43200\n\
NOCOMMWARNTIME 300\n\
FINALDELAY 5\n\
NOTIFYCMD /usr/sbin/upssched\n\
NOTIFYFLAG ONBATT SYSLOG+WALL+EXEC\n\
NOTIFYFLAG ONLINE SYSLOG+WALL+EXEC\n\
NOTIFYFLAG LOWBATT SYSLOG+WALL+EXEC\n\
NOTIFYFLAG SHUTDOWN SYSLOG+WALL+EXEC\n\
NOTIFYFLAG COMMBAD SYSLOG+EXEC\n\
NOTIFYFLAG COMMOK SYSLOG+EXEC\n\
NOTIFYFLAG NOCOMM SYSLOG+EXEC\n\
NOTIFYFLAG REPLBATT SYSLOG+EXEC\n"
    ))
}

pub fn render_upssched_conf(config: &ResolvedConfig) -> Result<String, RenderError> {
    let early = config.shutdown.early_after.as_secs();
    let notify = config.shutdown.notify_after.as_secs();
    let commbad = config.notify.commbad_after.as_secs();
    Ok(format!(
        "{MANAGED_HEADER}\n\
CMDSCRIPT {HOOK_PATH}\n\
PIPEFN /run/nut/upssched/upssched.pipe\n\
LOCKFN /run/nut/upssched/upssched.lock\n\
AT ONBATT * START-TIMER early-shutdown {early}\n\
AT ONLINE * CANCEL-TIMER early-shutdown\n\
AT ONBATT * START-TIMER onbatt {notify}\n\
AT ONLINE * CANCEL-TIMER onbatt online\n\
AT LOWBATT * EXECUTE lowbatt\n\
AT SHUTDOWN * EXECUTE shutdown\n\
AT COMMBAD * START-TIMER commbad {commbad}\n\
AT COMMOK * CANCEL-TIMER commbad commok\n\
AT NOCOMM * EXECUTE nocomm\n\
AT REPLBATT * EXECUTE replbatt\n"
    ))
}

pub fn render_hook(config: &ResolvedConfig) -> Result<String, RenderError> {
    let sender = validate_email("notify.from", &config.notify.from)?;
    let recipient = validate_email("notify.to", &config.notify.to)?;
    let ups = validate_ident("UPS name", &config.ups_name)?;
    let host = validate_ident("host name", &config.host.name)?;
    let early = config.shutdown.early_after.as_secs();
    let tag = JOURNAL_TAG;
    Ok(format!(
        r#"#!/bin/sh
{MANAGED_HEADER}
# upssched runs this as nut. Never invoke power events manually on the live host.
# Only offline tests may override PATH; every external command is then stubbed.
set -u
PATH=${{NUT_UPSSCHED_TEST_PATH:-/usr/sbin:/usr/bin:/sbin:/bin}}
export PATH

event=${{1:-}}
case "$event" in
    early-shutdown)
        logger -t {tag} 'on battery for {early} s; starting NUT forced shutdown'
        exec upsmon -c fsd
        ;;
    onbatt) summary='on battery' ;;
    online) summary='power restored' ;;
    lowbatt) summary='battery low' ;;
    shutdown) summary='shutdown starting' ;;
    commbad) summary='communication lost' ;;
    commok) summary='communication restored' ;;
    nocomm) summary='UPS unavailable' ;;
    replbatt) summary='replace battery' ;;
    test-notify) summary='test notification' ;;
    *) logger -t {tag} "unknown event: $event"; exit 1 ;;
esac
logger -t {tag} "$event: $summary"

sender='{sender}'
recipient='{recipient}'
{{
    printf 'From: {host} UPS <%s>\nTo: %s\nSubject: {host} UPS: %s\n\n' "$sender" "$recipient" "$summary"
    printf 'Event: %s (%s)\nTime: %s\n\n' "$event" "$summary" "$(date --iso-8601=seconds)"
    if readings=$(timeout 5 upsc {ups}@localhost 2>/dev/null); then
        printf '%s\n' "$readings" | awk '/^(ups.status|battery.charge|battery.runtime|ups.load|input.voltage):/'
    else
        printf 'UPS readings unavailable (upsc failed or timed out).\n'
    fi
}} | timeout 15 sendmail -f "$sender" -t
result=$?
if [ "$result" -ne 0 ]; then
    logger -t {tag} "mail submission failed for $event (exit $result)"
fi
exit 0
"#,
    ))
}

pub fn mask_secrets(text: &str) -> String {
    let mut lines = Vec::new();
    for line in text.lines() {
        lines.push(mask_line(line));
    }
    let mut out = lines.join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn mask_line(line: &str) -> String {
    let trimmed = line.trim_start();
    if trimmed.to_ascii_lowercase().starts_with("password")
        && let Some(index) = line.find('=')
    {
        return format!("{} ***", &line[..=index]);
    }
    if trimmed.starts_with("MONITOR ") {
        let mut parts = line.split_whitespace().collect::<Vec<_>>();
        if parts.len() >= 5 {
            parts[4] = "***";
            let indent_len = line.len() - line.trim_start().len();
            let indent = &line[..indent_len];
            return format!("{indent}{}", parts.join(" "));
        }
    }
    line.to_owned()
}

pub fn active_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(ToOwned::to_owned)
        .collect()
}

/// Compare rendered active lines with a deployed file after applying documented deltas.
pub fn normalize_deployed_active_lines(name: &str, text: &str) -> Vec<String> {
    let mut lines = active_lines(text);
    match name {
        "upssched.conf" => {
            for line in &mut lines {
                *line = line.replace("/usr/local/sbin/athena-upssched-cmd", HOOK_PATH);
            }
        }
        "upsd.users" => {
            if !lines
                .iter()
                .any(|line| line == &format!("[{OPERATOR_USER}]"))
            {
                lines.push(format!("[{OPERATOR_USER}]"));
                lines.push(format!("password = {OPERATOR_PLACEHOLDER}"));
                for cmd in OPERATOR_INSTCMDS {
                    lines.push(format!("instcmds = {cmd}"));
                }
            } else {
                // Drop any legacy operator grant that must not survive adoption.
                lines.retain(|line| line.trim() != "actions = SET");
                let combined = format!("instcmds = {}", OPERATOR_INSTCMDS.join(", "));
                lines.retain(|line| line.trim() != combined.trim());
                for cmd in OPERATOR_INSTCMDS {
                    let wanted = format!("instcmds = {cmd}");
                    if !lines.iter().any(|line| line.trim() == wanted) {
                        lines.push(wanted);
                    }
                }
            }
        }
        HOOK_NAME | "athena-upssched-cmd" => {
            lines = lines
                .into_iter()
                .map(|line| {
                    line.replace("athena-upssched", JOURNAL_TAG)
                        .replace("ATHENA_UPSSCHED_TEST_PATH", "NUT_UPSSCHED_TEST_PATH")
                })
                .collect();
        }
        _ => {}
    }
    lines
}

pub fn diff_live<H: Host>(host: &H, files: &RenderedFiles) -> Result<DiffReport, NutError> {
    let mut report = Vec::new();
    for (name, live_path) in LIVE_PATHS {
        let rendered = files.get(name).expect("known rendered file");
        let status = match host.read_file(Path::new(live_path)) {
            Ok(None) => DiffStatus::Missing,
            Err(error) if error.is_permission_denied() => DiffStatus::NeedsRoot,
            Err(error) => return Err(error.into()),
            Ok(Some(bytes)) => {
                let live = String::from_utf8_lossy(&bytes);
                let left = mask_secrets(&live);
                let right = mask_secrets(rendered);
                if left == right {
                    DiffStatus::Identical
                } else {
                    DiffStatus::Differs {
                        diff: unified_diff(live_path, &left, &right),
                    }
                }
            }
        };
        report.push(FileDiff {
            name,
            live_path,
            status,
        });
    }
    Ok(DiffReport { files: report })
}

fn unified_diff(path: &str, live: &str, rendered: &str) -> String {
    let diff = TextDiff::from_lines(live, rendered);
    let mut out = String::new();
    out.push_str(&format!("--- {path}\n+++ rendered\n"));
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Delete => "-",
            ChangeTag::Insert => "+",
            ChangeTag::Equal => " ",
        };
        out.push_str(&format!("{sign}{change}"));
    }
    out
}

fn default_maxretry(driver: &str) -> Option<String> {
    (driver == "usbhid-ups").then(|| "3".to_owned())
}

fn default_retrydelay(driver: &str) -> Option<String> {
    (driver == "usbhid-ups").then(|| "5".to_owned())
}

fn validate_ident<'a>(what: &'static str, value: &'a str) -> Result<&'a str, RenderError> {
    let valid = value
        .as_bytes()
        .first()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if valid {
        Ok(value)
    } else {
        Err(RenderError::UnsafeValue {
            what,
            value: value.to_owned(),
        })
    }
}

fn validate_token<'a>(what: &'static str, value: &'a str) -> Result<&'a str, RenderError> {
    if !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'_' | b'-' | b'/' | b'@' | b':' | b'+')
        })
    {
        Ok(value)
    } else {
        Err(RenderError::UnsafeValue {
            what,
            value: value.to_owned(),
        })
    }
}

fn validate_email<'a>(what: &'static str, value: &'a str) -> Result<&'a str, RenderError> {
    let Some((local, domain)) = value.split_once('@') else {
        return Err(RenderError::UnsafeValue {
            what,
            value: value.to_owned(),
        });
    };
    if local.is_empty()
        || domain.is_empty()
        || !domain.contains('.')
        || validate_token(what, value).is_err()
    {
        return Err(RenderError::UnsafeValue {
            what,
            value: value.to_owned(),
        });
    }
    Ok(value)
}

fn validate_description(value: &str) -> Result<&str, RenderError> {
    if value.is_empty()
        || value.contains(['"', '\n', '\r', '#', '`', '$', '\\'])
        || value.chars().any(|character| character.is_control())
    {
        return Err(RenderError::UnsafeValue {
            what: "description",
            value: value.to_owned(),
        });
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HostConfig, resolve};
    use crate::host::FakeHost;
    use crate::profiles::load_catalog;
    use std::path::PathBuf;

    const ATHENA: &str = include_str!("../tests/fixtures/hosts/athena/nut.toml");

    fn athena_rendered() -> RenderedFiles {
        let config = HostConfig::parse(ATHENA).unwrap();
        let host = FakeHost::default();
        let catalog = load_catalog(&host).unwrap();
        let resolved = resolve(
            PathBuf::from("tests/fixtures/hosts/athena/nut.toml"),
            &config,
            &catalog,
        )
        .unwrap();
        render(&resolved).unwrap()
    }

    #[test]
    fn dummy_ups_conf_omits_usb_ids() {
        let config = HostConfig::parse(include_str!("../tests/fixtures/e2e/host.toml")).unwrap();
        let host = FakeHost::default();
        let catalog = load_catalog(&host).unwrap();
        let resolved = resolve(PathBuf::from("/e2e.toml"), &config, &catalog).unwrap();
        let rendered = render_ups_conf(&resolved).unwrap();
        assert!(rendered.contains("driver = dummy-ups"));
        assert!(rendered.contains("port = /var/lib/nut/e2e.dev"));
        assert!(!rendered.contains("vendorid"));
        assert!(!rendered.contains("productid"));
        // NUT 2.8.1 dummy-ups exits on unknown driver variables.
        assert!(!rendered.contains("offdelay"));
        assert!(!rendered.contains("ondelay"));
    }

    #[test]
    fn masks_password_assignments_and_monitor_credentials() {
        let text = "password = hunter2\nMONITOR cp1500@localhost 1 upsmon s3cret primary\n";
        let masked = mask_secrets(text);
        assert!(masked.contains("password = ***"));
        assert!(masked.contains("MONITOR cp1500@localhost 1 upsmon *** primary"));
        assert!(!masked.contains("hunter2"));
        assert!(!masked.contains("s3cret"));
    }

    #[test]
    fn rejects_unsafe_ups_names_at_parse_time() {
        let input = ATHENA.replace("[ups.cp1500]", "[ups.\"cp1500 EXECUTE pwn\"]");
        assert!(HostConfig::parse(&input).is_err());
    }

    #[test]
    fn rejects_quotes_in_notify_addresses() {
        let mut text = ATHENA.to_owned();
        text = text.replace(
            "ups@example.invalid",
            r#"ups@example.invalid'; rm -rf /; echo '"#,
        );
        let config = HostConfig::parse(&text).unwrap();
        let host = FakeHost::default();
        let catalog = load_catalog(&host).unwrap();
        let resolved = resolve(PathBuf::from("x"), &config, &catalog).unwrap();
        assert!(render_hook(&resolved).is_err());
    }

    #[test]
    fn diff_marks_unreadable_live_files_as_needing_root() {
        let files = athena_rendered();
        let host = FakeHost::default()
            .with_unreadable("/etc/nut/nut.conf")
            .with_unreadable("/etc/nut/ups.conf")
            .with_unreadable("/etc/nut/upsd.users")
            .with_unreadable("/etc/nut/upsmon.conf")
            .with_unreadable("/etc/nut/upssched.conf")
            .with_file("/usr/local/sbin/nut-upssched-cmd", "old hook\n");
        let report = diff_live(&host, &files).unwrap();
        assert!(
            report
                .files
                .iter()
                .filter(|file| file.name != HOOK_NAME)
                .all(|file| matches!(file.status, DiffStatus::NeedsRoot))
        );
        assert!(matches!(
            report
                .files
                .iter()
                .find(|file| file.name == HOOK_NAME)
                .unwrap()
                .status,
            DiffStatus::Differs { .. }
        ));
        let text = report.render_text();
        assert!(text.contains("contents need root"));
    }
}
