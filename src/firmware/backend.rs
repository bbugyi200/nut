use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::NutError;
use crate::firmware::{AttestationFreshness, AttestationReceipt, read_trimmed};
use crate::host::Host;
use crate::profiles::{BiosBackendKind, BoardProfile, BoardSetting};

const FIRMWARE_ATTRIBUTES: &str = "/sys/class/firmware-attributes";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettingState {
    Unknown,
    Attested,
    Stale,
    Verified,
    Mismatch,
}

impl SettingState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Attested => "attested",
            Self::Stale => "stale",
            Self::Verified => "verified",
            Self::Mismatch => "mismatch",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettingReading {
    pub id: String,
    pub menu_path: Vec<String>,
    pub required: String,
    pub optional: bool,
    pub why: String,
    pub state: SettingState,
    pub current: Option<String>,
}

impl SettingReading {
    pub fn label(&self) -> &str {
        self.menu_path
            .last()
            .map(String::as_str)
            .unwrap_or(&self.id)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplyResult {
    pub wrote: usize,
    pub pending_reboot: bool,
}

pub trait BiosBackend {
    fn kind(&self) -> BiosBackendKind;
    fn read_settings<H: Host>(
        &self,
        host: &H,
        profile: &BoardProfile,
        receipt: Option<&AttestationReceipt>,
        freshness: &AttestationFreshness,
    ) -> Result<Vec<SettingReading>, NutError>;
    fn can_apply(&self) -> bool;
    fn apply<H: Host, W: Write>(
        &self,
        host: &H,
        profile: &BoardProfile,
        stdout: &mut W,
    ) -> Result<ApplyResult, NutError>;
}

pub enum SelectedBackend {
    Manual(ManualGuide),
    FirmwareAttributes(FirmwareAttributes),
}

impl SelectedBackend {
    pub fn kind(&self) -> BiosBackendKind {
        match self {
            Self::Manual(backend) => backend.kind(),
            Self::FirmwareAttributes(backend) => backend.kind(),
        }
    }

    pub fn read_settings<H: Host>(
        &self,
        host: &H,
        profile: &BoardProfile,
        receipt: Option<&AttestationReceipt>,
        freshness: &AttestationFreshness,
    ) -> Result<Vec<SettingReading>, NutError> {
        match self {
            Self::Manual(backend) => backend.read_settings(host, profile, receipt, freshness),
            Self::FirmwareAttributes(backend) => {
                backend.read_settings(host, profile, receipt, freshness)
            }
        }
    }

    pub fn can_apply(&self) -> bool {
        match self {
            Self::Manual(backend) => backend.can_apply(),
            Self::FirmwareAttributes(backend) => backend.can_apply(),
        }
    }

    pub fn apply<H: Host, W: Write>(
        &self,
        host: &H,
        profile: &BoardProfile,
        stdout: &mut W,
    ) -> Result<ApplyResult, NutError> {
        match self {
            Self::Manual(backend) => backend.apply(host, profile, stdout),
            Self::FirmwareAttributes(backend) => backend.apply(host, profile, stdout),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ManualGuide;

impl BiosBackend for ManualGuide {
    fn kind(&self) -> BiosBackendKind {
        BiosBackendKind::Manual
    }

    fn read_settings<H: Host>(
        &self,
        _host: &H,
        profile: &BoardProfile,
        receipt: Option<&AttestationReceipt>,
        freshness: &AttestationFreshness,
    ) -> Result<Vec<SettingReading>, NutError> {
        Ok(profile
            .settings
            .iter()
            .map(|setting| {
                let confirmed = receipt.and_then(|receipt| {
                    receipt
                        .answers
                        .iter()
                        .find(|answer| answer.id == setting.id)
                        .map(|answer| answer.confirmed)
                });
                let state = match (freshness, confirmed) {
                    (AttestationFreshness::Fresh, Some(true)) => SettingState::Attested,
                    (AttestationFreshness::Stale { .. }, Some(true)) => SettingState::Stale,
                    _ => SettingState::Unknown,
                };
                reading_from_setting(setting, state, None)
            })
            .collect())
    }

    fn can_apply(&self) -> bool {
        false
    }

    fn apply<H: Host, W: Write>(
        &self,
        _host: &H,
        _profile: &BoardProfile,
        _stdout: &mut W,
    ) -> Result<ApplyResult, NutError> {
        Err(NutError::operational(
            "this board uses the manual backend; set firmware at the keyboard with sudo nut bios setup",
        ))
    }
}

#[derive(Clone, Debug, Default)]
pub struct FirmwareAttributes {
    pub sysfs_root: PathBuf,
}

impl FirmwareAttributes {
    fn attributes_root(&self) -> PathBuf {
        if self.sysfs_root.as_os_str().is_empty() {
            PathBuf::from(FIRMWARE_ATTRIBUTES)
        } else {
            self.sysfs_root.join("class/firmware-attributes")
        }
    }

    fn class_dir<H: Host>(&self, host: &H) -> Result<Option<PathBuf>, NutError> {
        let root = self.attributes_root();
        let names = match host.list_dir(&root) {
            Ok(names) => names,
            Err(_) => return Ok(None),
        };
        Ok(names.into_iter().next().map(|name| root.join(name)))
    }

    fn attribute_path(class_dir: &Path, setting: &BoardSetting) -> PathBuf {
        let name = setting.attribute.as_deref().unwrap_or(setting.id.as_str());
        class_dir
            .join("attributes")
            .join(name)
            .join("current_value")
    }
}

impl BiosBackend for FirmwareAttributes {
    fn kind(&self) -> BiosBackendKind {
        BiosBackendKind::FirmwareAttributes
    }

    fn read_settings<H: Host>(
        &self,
        host: &H,
        profile: &BoardProfile,
        _receipt: Option<&AttestationReceipt>,
        _freshness: &AttestationFreshness,
    ) -> Result<Vec<SettingReading>, NutError> {
        let class_dir = self.class_dir(host)?;
        let mut readings = Vec::new();
        for setting in &profile.settings {
            let current = match &class_dir {
                Some(dir) => read_trimmed(host, &Self::attribute_path(dir, setting))?,
                None => None,
            };
            let state = match &current {
                Some(value) if value.eq_ignore_ascii_case(&setting.required) => {
                    SettingState::Verified
                }
                Some(_) => SettingState::Mismatch,
                None => SettingState::Unknown,
            };
            readings.push(reading_from_setting(setting, state, current));
        }
        Ok(readings)
    }

    fn can_apply(&self) -> bool {
        true
    }

    fn apply<H: Host, W: Write>(
        &self,
        host: &H,
        profile: &BoardProfile,
        stdout: &mut W,
    ) -> Result<ApplyResult, NutError> {
        if host.euid() != 0 {
            return Err(NutError::operational(
                "firmware-attributes apply needs root → sudo nut bios setup",
            ));
        }
        if !host.stdin_is_terminal() || !host.stdout_is_terminal() {
            return Err(NutError::operational(
                "firmware-attributes apply needs a TTY on stdin and stdout",
            ));
        }
        let Some(class_dir) = self.class_dir(host)? else {
            return Err(NutError::operational(
                "no /sys/class/firmware-attributes class is present on this machine",
            ));
        };
        let readings = self.read_settings(host, profile, None, &AttestationFreshness::Missing)?;
        let pending: Vec<(&BoardSetting, String)> = profile
            .settings
            .iter()
            .zip(readings.iter())
            .filter_map(|(setting, reading)| {
                if reading.state == SettingState::Verified {
                    None
                } else {
                    Some((setting, setting.required.clone()))
                }
            })
            .collect();
        writeln!(stdout, "Plan").map_err(write_error)?;
        if pending.is_empty() {
            writeln!(stdout, "  already at the required values").map_err(write_error)?;
            return Ok(ApplyResult {
                wrote: 0,
                pending_reboot: pending_reboot(host, &class_dir)?,
            });
        }
        for (setting, value) in &pending {
            writeln!(
                stdout,
                "  {} → {value}",
                Self::attribute_path(&class_dir, setting).display()
            )
            .map_err(write_error)?;
        }
        let answer = host.prompt_line("Apply these firmware-attribute writes? [y/N] ")?;
        if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
            return Err(NutError::operational("firmware-attributes apply cancelled"));
        }
        for (setting, value) in &pending {
            host.write_file(
                &Self::attribute_path(&class_dir, setting),
                format!("{value}\n").as_bytes(),
            )?;
        }
        Ok(ApplyResult {
            wrote: pending.len(),
            pending_reboot: pending_reboot(host, &class_dir)?,
        })
    }
}

fn pending_reboot<H: Host>(host: &H, class_dir: &Path) -> Result<bool, NutError> {
    let path = class_dir.join("attributes").join("pending_reboot");
    Ok(read_trimmed(host, &path)?
        .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true")))
}

fn reading_from_setting(
    setting: &BoardSetting,
    state: SettingState,
    current: Option<String>,
) -> SettingReading {
    SettingReading {
        id: setting.id.clone(),
        menu_path: setting.menu_path.clone(),
        required: setting.required.clone(),
        optional: setting.optional,
        why: setting.why.clone(),
        state,
        current,
    }
}

fn write_error(error: std::io::Error) -> NutError {
    NutError::operational(format!("cannot write output: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::firmware::AttestationAnswer;
    use crate::host::FakeHost;
    use crate::profiles::ProfileCatalog;

    fn trx40() -> BoardProfile {
        ProfileCatalog::with_embedded_boards()
            .unwrap()
            .board(&crate::profiles::ProfileId::new("asus-prime-trx40-pro").unwrap())
            .unwrap()
            .clone()
    }

    #[test]
    fn manual_guide_reports_attested_or_unknown_never_verified() {
        let profile = trx40();
        let receipt = AttestationReceipt {
            schema_version: 1,
            profile_id: profile.id.to_string(),
            board_vendor: "ASUSTeK COMPUTER INC.".into(),
            board_name: "PRIME TRX40-PRO".into(),
            bios_version: Some("1101".into()),
            bios_date: None,
            fingerprint: "abc".into(),
            answers: vec![AttestationAnswer {
                id: "ac-power-loss".into(),
                confirmed: true,
            }],
            recorded_unix: 1,
        };
        let host = FakeHost::default();
        let readings = ManualGuide
            .read_settings(
                &host,
                &profile,
                Some(&receipt),
                &AttestationFreshness::Fresh,
            )
            .unwrap();
        assert_eq!(readings[0].state, SettingState::Attested);
        assert!(
            readings
                .iter()
                .all(|reading| reading.state != SettingState::Verified)
        );
        let stale = ManualGuide
            .read_settings(
                &host,
                &profile,
                Some(&receipt),
                &AttestationFreshness::Stale {
                    reason: "efivar fingerprint changed".into(),
                },
            )
            .unwrap();
        assert_eq!(stale[0].state, SettingState::Stale);
        assert!(ManualGuide.apply(&host, &profile, &mut Vec::new()).is_err());
        assert!(!ManualGuide.can_apply());
    }

    #[test]
    fn firmware_attributes_reads_and_applies_against_fake_sysfs() {
        let mut profile = trx40();
        profile.backend = BiosBackendKind::FirmwareAttributes;
        profile.settings[0].attribute = Some("AcPwrRcvry".into());
        profile.settings.truncate(1);
        let host = FakeHost::default()
            .with_euid(0)
            .with_stdin_tty(true)
            .with_tty(true, true)
            .with_prompt_lines(["y"])
            .with_file(
                "/sys/class/firmware-attributes/dell/attributes/AcPwrRcvry/current_value",
                "LastState\n",
            )
            .with_file(
                "/sys/class/firmware-attributes/dell/attributes/pending_reboot",
                "0\n",
            );
        let backend = FirmwareAttributes::default();
        let before = backend
            .read_settings(&host, &profile, None, &AttestationFreshness::Missing)
            .unwrap();
        assert_eq!(before[0].state, SettingState::Mismatch);
        assert_eq!(before[0].current.as_deref(), Some("LastState"));
        let mut out = Vec::new();
        let result = backend.apply(&host, &profile, &mut out).unwrap();
        assert_eq!(result.wrote, 1);
        let after = backend
            .read_settings(&host, &profile, None, &AttestationFreshness::Missing)
            .unwrap();
        assert_eq!(after[0].state, SettingState::Verified);
        assert!(String::from_utf8(out).unwrap().contains("AcPwrRcvry"));
    }
}
