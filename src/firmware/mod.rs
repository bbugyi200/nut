pub mod audit;
pub mod backend;

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::HostConfig;
use crate::host::{Host, HostError};
use crate::profiles::{BoardProfile, ProfileCatalog, ProfileId};

pub const BIOS_STATE_DIR: &str = "/var/lib/nut-cli/bios";
pub const RECEIPT_NAME: &str = "attestation.json";
pub const PENDING_NAME: &str = "pending.json";
const EFIVARS_DIR: &str = "/sys/firmware/efi/efivars";
const DMI_DIR: &str = "/sys/class/dmi/id";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DmiInfo {
    pub board_vendor: String,
    pub board_name: String,
    pub bios_vendor: Option<String>,
    pub bios_version: Option<String>,
    pub bios_date: Option<String>,
}

impl DmiInfo {
    pub fn read<H: Host>(host: &H) -> Result<Self, HostError> {
        Ok(Self {
            board_vendor: read_trimmed(host, &PathBuf::from(DMI_DIR).join("board_vendor"))?
                .unwrap_or_else(|| "unknown".to_owned()),
            board_name: read_trimmed(host, &PathBuf::from(DMI_DIR).join("board_name"))?
                .unwrap_or_else(|| "unknown".to_owned()),
            bios_vendor: read_trimmed(host, &PathBuf::from(DMI_DIR).join("bios_vendor"))?,
            bios_version: read_trimmed(host, &PathBuf::from(DMI_DIR).join("bios_version"))?,
            bios_date: read_trimmed(host, &PathBuf::from(DMI_DIR).join("bios_date"))?,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AttestationReceipt {
    pub schema_version: u32,
    pub profile_id: String,
    pub board_vendor: String,
    pub board_name: String,
    pub bios_version: Option<String>,
    pub bios_date: Option<String>,
    pub fingerprint: String,
    pub answers: Vec<AttestationAnswer>,
    pub recorded_unix: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AttestationAnswer {
    pub id: String,
    pub confirmed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PendingMarker {
    pub schema_version: u32,
    pub profile_id: String,
    pub bios_version: Option<String>,
    pub recorded_unix: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AttestationFreshness {
    Missing,
    Pending,
    Fresh,
    Stale { reason: String },
}

impl AttestationFreshness {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Pending => "pending",
            Self::Fresh => "fresh",
            Self::Stale { .. } => "stale",
        }
    }
}

pub fn read_trimmed<H: Host>(host: &H, path: &Path) -> Result<Option<String>, HostError> {
    let Some(bytes) = host.read_file(path)? else {
        return Ok(None);
    };
    let text = String::from_utf8_lossy(&bytes);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_owned()))
    }
}

pub fn unix_secs(now: SystemTime) -> u64 {
    now.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

pub fn glob_match(pattern: &str, name: &str) -> bool {
    let mut name_bytes = name.as_bytes();
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == name;
    }
    if !parts[0].is_empty() {
        let prefix = parts[0].as_bytes();
        if !name_bytes.starts_with(prefix) {
            return false;
        }
        name_bytes = &name_bytes[prefix.len()..];
    }
    for (index, part) in parts.iter().enumerate().skip(1) {
        if part.is_empty() {
            if index == parts.len() - 1 {
                return true;
            }
            continue;
        }
        let needle = part.as_bytes();
        if index == parts.len() - 1 {
            return name_bytes.ends_with(needle);
        }
        let Some(found) = name_bytes
            .windows(needle.len())
            .position(|window| window == needle)
        else {
            return false;
        };
        name_bytes = &name_bytes[found + needle.len()..];
    }
    true
}

pub fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn efivar_fingerprint<H: Host>(
    host: &H,
    globs: &[String],
) -> Result<Option<String>, HostError> {
    if globs.is_empty() {
        return Ok(None);
    }
    let names = match host.list_dir(Path::new(EFIVARS_DIR)) {
        Ok(names) => names,
        Err(_) => return Ok(None),
    };
    let mut matched: Vec<String> = names
        .into_iter()
        .filter(|name| globs.iter().any(|pattern| glob_match(pattern, name)))
        .collect();
    if matched.is_empty() {
        return Ok(None);
    }
    matched.sort();
    let mut material = Vec::new();
    for name in matched {
        material.extend_from_slice(name.as_bytes());
        material.push(0);
        if let Some(bytes) = host.read_file(&PathBuf::from(EFIVARS_DIR).join(&name))? {
            material.extend_from_slice(&bytes);
        }
        material.push(0);
    }
    Ok(Some(hex_sha256(&material)))
}

pub fn firmware_setup_supported<H: Host>(host: &H) -> Result<bool, HostError> {
    let names = match host.list_dir(Path::new(EFIVARS_DIR)) {
        Ok(names) => names,
        Err(_) => return Ok(false),
    };
    for name in names {
        if !glob_match("OsIndicationsSupported-*", &name) {
            continue;
        }
        let Some(bytes) = host.read_file(&PathBuf::from(EFIVARS_DIR).join(&name))? else {
            continue;
        };
        if bytes.len() >= 12 {
            let mut value = [0u8; 8];
            value.copy_from_slice(&bytes[4..12]);
            return Ok(u64::from_le_bytes(value) & 1 == 1);
        }
        if bytes.len() >= 8 {
            let mut value = [0u8; 4];
            value.copy_from_slice(&bytes[4..8]);
            return Ok(u32::from_le_bytes(value) & 1 == 1);
        }
    }
    Ok(false)
}

pub fn load_board_catalog<H: Host>(
    host: &H,
) -> Result<ProfileCatalog, crate::profiles::ProfileError> {
    let mut catalog = ProfileCatalog::with_embedded_boards()?;
    catalog.load_board_overrides(host)?;
    Ok(catalog)
}

pub fn resolve_profile<'a>(
    catalog: &'a ProfileCatalog,
    config: Option<&HostConfig>,
    dmi: &DmiInfo,
) -> &'a BoardProfile {
    if let Some(id) = config.and_then(|config| config.boot.board_profile.as_ref())
        && let Some(profile) = catalog.board(id)
    {
        return profile;
    }
    catalog
        .match_board(&dmi.board_vendor, &dmi.board_name)
        .or_else(|| catalog.board(&ProfileId::new("generic").expect("generic id is valid")))
        .expect("embedded generic board profile")
}

pub fn receipt_path() -> PathBuf {
    PathBuf::from(BIOS_STATE_DIR).join(RECEIPT_NAME)
}

pub fn pending_path() -> PathBuf {
    PathBuf::from(BIOS_STATE_DIR).join(PENDING_NAME)
}

pub fn load_receipt<H: Host>(host: &H) -> Result<Option<AttestationReceipt>, HostError> {
    let Some(bytes) = host.read_file(&receipt_path())? else {
        return Ok(None);
    };
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let receipt = serde_json::from_slice(&bytes)
        .map_err(|error| HostError::message(format!("cannot parse BIOS attestation: {error}")))?;
    Ok(Some(receipt))
}

pub fn load_pending<H: Host>(host: &H) -> Result<Option<PendingMarker>, HostError> {
    let Some(bytes) = host.read_file(&pending_path())? else {
        return Ok(None);
    };
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let pending = serde_json::from_slice(&bytes).map_err(|error| {
        HostError::message(format!("cannot parse pending BIOS marker: {error}"))
    })?;
    Ok(Some(pending))
}

pub fn save_json<H: Host, T: Serialize>(host: &H, path: &Path, value: &T) -> Result<(), HostError> {
    host.create_dir_all(Path::new(BIOS_STATE_DIR))?;
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|error| HostError::message(format!("cannot encode BIOS state: {error}")))?;
    host.write_file(path, &bytes)
}

pub fn clear_pending<H: Host>(host: &H) -> Result<(), HostError> {
    host.write_file(&pending_path(), b"")
}

pub fn freshness(
    pending: Option<&PendingMarker>,
    receipt: Option<&AttestationReceipt>,
    dmi: &DmiInfo,
    fingerprint: Option<&str>,
) -> AttestationFreshness {
    if pending.is_some() {
        return AttestationFreshness::Pending;
    }
    let Some(receipt) = receipt else {
        return AttestationFreshness::Missing;
    };
    if receipt.bios_version != dmi.bios_version {
        return AttestationFreshness::Stale {
            reason: format!(
                "BIOS version changed ({} → {})",
                receipt.bios_version.as_deref().unwrap_or("unknown"),
                dmi.bios_version.as_deref().unwrap_or("unknown")
            ),
        };
    }
    match (fingerprint, receipt.fingerprint.as_str()) {
        (Some(live), recorded) if live != recorded => AttestationFreshness::Stale {
            reason: "efivar fingerprint changed".to_owned(),
        },
        _ => AttestationFreshness::Fresh,
    }
}

pub fn backend_for(profile: &BoardProfile) -> backend::SelectedBackend {
    match profile.backend {
        crate::profiles::BiosBackendKind::Manual => {
            backend::SelectedBackend::Manual(backend::ManualGuide)
        }
        crate::profiles::BiosBackendKind::FirmwareAttributes => {
            backend::SelectedBackend::FirmwareAttributes(backend::FirmwareAttributes::default())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::FakeHost;

    #[test]
    fn glob_match_supports_prefix_wildcards() {
        assert!(glob_match(
            "Setup-*",
            "Setup-ec87d643-eba4-4bb5-a1e5-3f3e36b20da9"
        ));
        assert!(glob_match(
            "AmdSetup-*",
            "AmdSetup-3a997502-647a-4c82-998e-52ef9486a247"
        ));
        assert!(!glob_match("Setup-*", "AmdSetup-abc"));
        assert!(glob_match(
            "OsIndicationsSupported-*",
            "OsIndicationsSupported-8be4df61"
        ));
    }

    #[test]
    fn fingerprint_hashes_matched_efivars_in_name_order() {
        let host = FakeHost::default()
            .with_file(
                "/sys/firmware/efi/efivars/Setup-aaaa",
                b"\x07\x00\x00\x00setup",
            )
            .with_file(
                "/sys/firmware/efi/efivars/AmdSetup-bbbb",
                b"\x07\x00\x00\x00amd",
            )
            .with_file("/sys/firmware/efi/efivars/Ignored-cccc", b"nope");
        let hash = efivar_fingerprint(&host, &["Setup-*".into(), "AmdSetup-*".into()])
            .unwrap()
            .unwrap();
        assert_eq!(hash.len(), 64);
        let changed = FakeHost::default()
            .with_file(
                "/sys/firmware/efi/efivars/Setup-aaaa",
                b"\x07\x00\x00\x00SETUP",
            )
            .with_file(
                "/sys/firmware/efi/efivars/AmdSetup-bbbb",
                b"\x07\x00\x00\x00amd",
            );
        let other = efivar_fingerprint(&changed, &["Setup-*".into(), "AmdSetup-*".into()])
            .unwrap()
            .unwrap();
        assert_ne!(hash, other);
    }

    #[test]
    fn firmware_setup_reads_os_indications_bit() {
        let mut value = vec![0x07, 0, 0, 0];
        value.extend_from_slice(&1u64.to_le_bytes());
        let host = FakeHost::default().with_file(
            "/sys/firmware/efi/efivars/OsIndicationsSupported-8be4df61-93ca-11d2-aa0d-00e098032b8c",
            value,
        );
        assert!(firmware_setup_supported(&host).unwrap());
    }

    #[test]
    fn freshness_is_a_warning_when_the_fingerprint_changes() {
        let dmi = DmiInfo {
            board_vendor: "ASUSTeK COMPUTER INC.".into(),
            board_name: "PRIME TRX40-PRO".into(),
            bios_vendor: None,
            bios_version: Some("1101".into()),
            bios_date: Some("07/01/2020".into()),
        };
        let receipt = AttestationReceipt {
            schema_version: 1,
            profile_id: "asus-prime-trx40-pro".into(),
            board_vendor: dmi.board_vendor.clone(),
            board_name: dmi.board_name.clone(),
            bios_version: Some("1101".into()),
            bios_date: dmi.bios_date.clone(),
            fingerprint: "abc".into(),
            answers: Vec::new(),
            recorded_unix: 1,
        };
        match freshness(None, Some(&receipt), &dmi, Some("def")) {
            AttestationFreshness::Stale { reason } => {
                assert!(reason.contains("fingerprint"));
            }
            other => panic!("expected stale, got {other:?}"),
        }
        assert!(matches!(
            freshness(None, Some(&receipt), &dmi, Some("abc")),
            AttestationFreshness::Fresh
        ));
    }
}
