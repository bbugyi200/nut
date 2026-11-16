use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::duration::DurationValue;
use crate::host::Host;

const UPS_OVERRIDE_DIR: &str = "/etc/nut-cli/ups";
const BOARD_OVERRIDE_DIR: &str = "/etc/nut-cli/boards";
const BOARD_SCHEMA: u32 = 1;
const EMBEDDED_BOARD_PROFILES: &[(&str, &str)] = &[
    (
        "asus-prime-trx40-pro",
        include_str!("../profiles/boards/asus-prime-trx40-pro.toml"),
    ),
    ("generic", include_str!("../profiles/boards/generic.toml")),
];

/// Stable kebab-case identifier shared by host settings and embedded profile catalogs.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct ProfileId(String);

impl ProfileId {
    pub fn new(value: impl Into<String>) -> Result<Self, ProfileError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.as_bytes()[0].is_ascii_lowercase()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            && !value.ends_with('-')
            && !value.contains("--");
        if !valid {
            return Err(ProfileError::InvalidId(value));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProfileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for ProfileId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ProfileId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UsbId {
    pub vendor: u16,
    pub product: u16,
}

impl UsbId {
    pub fn parse(value: &str) -> Result<Self, ProfileError> {
        let (vendor, product) = value
            .split_once(':')
            .ok_or_else(|| ProfileError::InvalidUsbId(value.to_owned()))?;
        if vendor.len() != 4 || product.len() != 4 {
            return Err(ProfileError::InvalidUsbId(value.to_owned()));
        }
        let vendor = u16::from_str_radix(vendor, 16)
            .map_err(|_| ProfileError::InvalidUsbId(value.to_owned()))?;
        let product = u16::from_str_radix(product, 16)
            .map_err(|_| ProfileError::InvalidUsbId(value.to_owned()))?;
        Ok(Self { vendor, product })
    }

    pub fn vendor_hex(self) -> String {
        format!("{:04x}", self.vendor)
    }

    pub fn product_hex(self) -> String {
        format!("{:04x}", self.product)
    }
}

impl fmt::Display for UsbId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:04x}:{:04x}", self.vendor, self.product)
    }
}

impl Serialize for UsbId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for UsbId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpsProfile {
    pub schema: u32,
    pub id: ProfileId,
    pub manufacturer: String,
    pub model: String,
    #[serde(default)]
    pub model_aliases: Vec<String>,
    pub driver: String,
    #[serde(default)]
    pub usb_ids: Vec<UsbId>,
    pub offdelay: Option<DurationValue>,
    pub ondelay: Option<DurationValue>,
    #[serde(default)]
    pub offdelay_rationale: Option<String>,
    #[serde(default)]
    pub ondelay_rationale: Option<String>,
    pub nominal_va: Option<u32>,
    pub nominal_watts: Option<u32>,
    #[serde(default)]
    pub cartridges: Vec<String>,
}

impl UpsProfile {
    pub fn parse(contents: &str) -> Result<Self, ProfileError> {
        let profile: Self = toml::from_str(contents)
            .map_err(|error| ProfileError::InvalidToml(error.to_string()))?;
        if profile.schema != 1 {
            return Err(ProfileError::UnsupportedSchema(profile.schema));
        }
        Ok(profile)
    }

    pub fn matches_usb(&self, id: UsbId) -> bool {
        self.usb_ids.contains(&id)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BiosBackendKind {
    Manual,
    FirmwareAttributes,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardMatch {
    pub board_vendor: String,
    #[serde(deserialize_with = "deserialize_one_or_many")]
    pub board_name: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirmwareKeys {
    pub setup: String,
    #[serde(default)]
    pub advanced_mode: Option<String>,
    pub save: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardSetting {
    pub id: String,
    pub menu_path: Vec<String>,
    pub required: String,
    #[serde(default)]
    pub optional: bool,
    pub why: String,
    #[serde(default)]
    pub attribute: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardProfile {
    pub schema: u32,
    pub id: ProfileId,
    #[serde(rename = "match", default)]
    pub match_: Option<BoardMatch>,
    pub keys: FirmwareKeys,
    #[serde(default)]
    pub fingerprint_efivars: Vec<String>,
    pub backend: BiosBackendKind,
    #[serde(default)]
    pub settings: Vec<BoardSetting>,
    #[serde(default)]
    pub caveats: Vec<String>,
    #[serde(default)]
    pub sources: Vec<String>,
}

impl BoardProfile {
    pub fn parse(contents: &str) -> Result<Self, ProfileError> {
        let profile: Self = toml::from_str(contents)
            .map_err(|error| ProfileError::InvalidToml(error.to_string()))?;
        if profile.schema != BOARD_SCHEMA {
            return Err(ProfileError::UnsupportedSchema(profile.schema));
        }
        Ok(profile)
    }

    pub fn matches(&self, vendor: &str, board_name: &str) -> bool {
        let Some(spec) = &self.match_ else {
            return false;
        };
        spec.board_vendor.eq_ignore_ascii_case(vendor)
            && spec
                .board_name
                .iter()
                .any(|name| name.eq_ignore_ascii_case(board_name))
    }

    pub fn display_name(&self) -> &str {
        self.match_
            .as_ref()
            .and_then(|spec| spec.board_name.first())
            .map(String::as_str)
            .unwrap_or_else(|| self.id.as_str())
    }
}

fn deserialize_one_or_many<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(value) => Ok(vec![value]),
        OneOrMany::Many(values) => Ok(values),
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ProfileError {
    #[error("invalid profile identifier {0:?}; use lowercase letters, digits, and single hyphens")]
    InvalidId(String),
    #[error("invalid USB identifier {0:?}; expected four hex digits, a colon, and four hex digits")]
    InvalidUsbId(String),
    #[error("duplicate profile identifier {0}")]
    Duplicate(ProfileId),
    #[error("unsupported profile schema version {0}; this build supports version 1")]
    UnsupportedSchema(u32),
    #[error("invalid profile TOML: {0}")]
    InvalidToml(String),
    #[error("cannot load profile overrides: {0}")]
    Host(String),
}

#[derive(Clone, Debug, Default)]
pub struct ProfileCatalog {
    ups: BTreeMap<ProfileId, UpsProfile>,
    boards: BTreeMap<ProfileId, BoardProfile>,
}

impl ProfileCatalog {
    pub fn insert_ups(&mut self, profile: UpsProfile) -> Result<(), ProfileError> {
        if self.ups.contains_key(&profile.id) {
            return Err(ProfileError::Duplicate(profile.id));
        }
        self.ups.insert(profile.id.clone(), profile);
        Ok(())
    }

    pub fn replace_ups(&mut self, profile: UpsProfile) {
        self.ups.insert(profile.id.clone(), profile);
    }

    pub fn insert_board(&mut self, profile: BoardProfile) -> Result<(), ProfileError> {
        if self.boards.contains_key(&profile.id) {
            return Err(ProfileError::Duplicate(profile.id));
        }
        self.boards.insert(profile.id.clone(), profile);
        Ok(())
    }

    pub fn replace_board(&mut self, profile: BoardProfile) {
        self.boards.insert(profile.id.clone(), profile);
    }

    pub fn ups(&self, id: &ProfileId) -> Option<&UpsProfile> {
        self.ups.get(id)
    }

    pub fn board(&self, id: &ProfileId) -> Option<&BoardProfile> {
        self.boards.get(id)
    }

    pub fn ups_profiles(&self) -> impl Iterator<Item = &UpsProfile> {
        self.ups.values()
    }

    pub fn board_profiles(&self) -> impl Iterator<Item = &BoardProfile> {
        self.boards.values()
    }

    pub fn with_embedded_boards() -> Result<Self, ProfileError> {
        let mut catalog = Self::default();
        for (_, contents) in EMBEDDED_BOARD_PROFILES {
            catalog.insert_board(BoardProfile::parse(contents)?)?;
        }
        Ok(catalog)
    }

    pub fn load_board_overrides<H: Host>(&mut self, host: &H) -> Result<(), ProfileError> {
        overlay_boards(host, self, Path::new(BOARD_OVERRIDE_DIR))
    }

    pub fn match_usb(&self, id: UsbId) -> Vec<&UpsProfile> {
        self.ups
            .values()
            .filter(|profile| profile.matches_usb(id))
            .collect()
    }

    pub fn match_board(&self, vendor: &str, board_name: &str) -> Option<&BoardProfile> {
        self.boards
            .values()
            .find(|profile| profile.matches(vendor, board_name))
            .or_else(|| self.board(&ProfileId::new("generic").ok()?))
    }
}

pub fn embedded_catalog() -> Result<ProfileCatalog, ProfileError> {
    let mut catalog = ProfileCatalog::with_embedded_boards()?;
    catalog.insert_ups(UpsProfile::parse(include_str!(
        "../profiles/ups/cyberpower-cp1500pfclcd.toml"
    ))?)?;
    catalog.insert_ups(UpsProfile::parse(include_str!(
        "../profiles/ups/usbhid-generic.toml"
    ))?)?;
    catalog.insert_ups(UpsProfile::parse(include_str!(
        "../profiles/ups/dummy.toml"
    ))?)?;
    Ok(catalog)
}

pub fn load_catalog<H: Host>(host: &H) -> Result<ProfileCatalog, ProfileError> {
    let mut catalog = embedded_catalog()?;
    overlay_ups(host, &mut catalog, Path::new(UPS_OVERRIDE_DIR))?;
    overlay_boards(host, &mut catalog, Path::new(BOARD_OVERRIDE_DIR))?;
    Ok(catalog)
}

fn overlay_ups<H: Host>(
    host: &H,
    catalog: &mut ProfileCatalog,
    dir: &Path,
) -> Result<(), ProfileError> {
    let names = match host.list_dir(dir) {
        Ok(names) => names,
        Err(_) => return Ok(()),
    };
    for name in names {
        if !name.ends_with(".toml") {
            continue;
        }
        let path = dir.join(&name);
        let Some(bytes) = host
            .read_file(&path)
            .map_err(|error| ProfileError::Host(error.to_string()))?
        else {
            continue;
        };
        let contents = std::str::from_utf8(&bytes)
            .map_err(|_| ProfileError::InvalidToml(format!("{} is not UTF-8", path.display())))?;
        catalog.replace_ups(UpsProfile::parse(contents)?);
    }
    Ok(())
}

fn overlay_boards<H: Host>(
    host: &H,
    catalog: &mut ProfileCatalog,
    dir: &Path,
) -> Result<(), ProfileError> {
    let names = match host.list_dir(dir) {
        Ok(names) => names,
        Err(_) => return Ok(()),
    };
    for name in names {
        if !name.ends_with(".toml") {
            continue;
        }
        let path = dir.join(&name);
        let Some(bytes) = host
            .read_file(&path)
            .map_err(|error| ProfileError::Host(error.to_string()))?
        else {
            continue;
        };
        let contents = std::str::from_utf8(&bytes)
            .map_err(|_| ProfileError::InvalidToml(format!("{} is not UTF-8", path.display())))?;
        catalog.replace_board(BoardProfile::parse(contents)?);
    }
    Ok(())
}

pub fn read_dmi<H: Host>(host: &H) -> Result<(Option<String>, Option<String>), ProfileError> {
    Ok((
        read_sys_text(host, Path::new("/sys/class/dmi/id/board_vendor"))?,
        read_sys_text(host, Path::new("/sys/class/dmi/id/board_name"))?,
    ))
}

pub fn discover_usb_ids<H: Host>(host: &H) -> Result<Vec<(PathBuf, UsbId)>, ProfileError> {
    let root = Path::new("/sys/bus/usb/devices");
    let names = match host.list_dir(root) {
        Ok(names) => names,
        Err(_) => return Ok(Vec::new()),
    };
    let mut found = Vec::new();
    for name in names {
        let device = root.join(&name);
        let Some(vendor) = read_sys_text(host, &device.join("idVendor"))? else {
            continue;
        };
        let Some(product) = read_sys_text(host, &device.join("idProduct"))? else {
            continue;
        };
        if vendor.len() != 4 || product.len() != 4 {
            continue;
        }
        if let Ok(id) = UsbId::parse(&format!("{vendor}:{product}")) {
            found.push((device, id));
        }
    }
    Ok(found)
}

fn read_sys_text<H: Host>(host: &H, path: &Path) -> Result<Option<String>, ProfileError> {
    match host.read_file(path) {
        Ok(Some(bytes)) => {
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| ProfileError::InvalidToml(format!("{} is not UTF-8", path.display())))?
                .trim()
                .to_owned();
            if text.is_empty() {
                Ok(None)
            } else {
                Ok(Some(text))
            }
        }
        Ok(None) => Ok(None),
        Err(error) if error.is_permission_denied() => Ok(None),
        Err(error) => Err(ProfileError::Host(error.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::FakeHost;

    #[test]
    fn profile_ids_are_stable_slugs() {
        assert_eq!(
            ProfileId::new("cyberpower-cp1500").unwrap().as_str(),
            "cyberpower-cp1500"
        );
        assert!(ProfileId::new("CyberPower").is_err());
        assert!(ProfileId::new("has--double").is_err());
    }

    #[test]
    fn usb_ids_parse_and_render_as_four_digit_hex_pairs() {
        let id = UsbId::parse("0764:0601").unwrap();
        assert_eq!(id.vendor, 0x0764);
        assert_eq!(id.to_string(), "0764:0601");
        assert!(UsbId::parse("764:601").is_err());
    }

    #[test]
    fn board_match_uses_board_dmi_fields_case_insensitively() {
        let profile =
            BoardProfile::parse(include_str!("../profiles/boards/asus-prime-trx40-pro.toml"))
                .unwrap();
        assert!(profile.matches("asustek computer inc.", "prime trx40-pro"));
        assert!(!profile.matches("ASUSTeK COMPUTER INC.", "System Product Name"));
        assert_eq!(profile.backend, BiosBackendKind::Manual);
        assert_eq!(profile.settings[0].id, "ac-power-loss");
        assert!(profile.settings.iter().any(|setting| setting.optional));
    }

    #[test]
    fn generic_profile_is_the_fallback_and_never_auto_matches() {
        let catalog = ProfileCatalog::with_embedded_boards().unwrap();
        let generic = catalog.board(&ProfileId::new("generic").unwrap()).unwrap();
        assert!(!generic.matches("ASUSTeK COMPUTER INC.", "PRIME TRX40-PRO"));
        let matched = catalog
            .match_board("ASUSTeK COMPUTER INC.", "PRIME TRX40-PRO")
            .unwrap();
        assert_eq!(matched.id.as_str(), "asus-prime-trx40-pro");
        let fallback = catalog
            .match_board("Unknown Vendor", "Mystery Board")
            .unwrap();
        assert_eq!(fallback.id.as_str(), "generic");
        assert!(
            generic
                .caveats
                .iter()
                .any(|caveat| caveat.contains("Last State"))
        );
    }

    #[test]
    fn embedded_catalog_includes_athena_hardware() {
        let catalog = embedded_catalog().unwrap();
        let ups = catalog
            .ups(&ProfileId::new("cyberpower-cp1500pfclcd").unwrap())
            .unwrap();
        assert!(ups.matches_usb(UsbId::parse("0764:0601").unwrap()));
        assert!(ups.matches_usb(UsbId::parse("0764:0501").unwrap()));
        assert_eq!(ups.offdelay.unwrap().as_secs(), 60);
        assert_eq!(ups.ondelay.unwrap().as_secs(), 0);
        assert_eq!(ups.nominal_watts, Some(1000));
        assert!(ups.cartridges.contains(&"RB1290X2".to_owned()));
        let board = catalog
            .board(&ProfileId::new("asus-prime-trx40-pro").unwrap())
            .unwrap();
        assert!(board.matches("asustek computer inc.", "prime trx40-pro"));
        assert!(!board.matches("ASUSTeK COMPUTER INC.", "System Product Name"));
        assert!(catalog.board(&ProfileId::new("generic").unwrap()).is_some());
    }

    #[test]
    fn overlay_replaces_embedded_ups_profile() {
        let host = FakeHost::default().with_file(
            "/etc/nut-cli/ups/dummy.toml",
            r#"
schema = 1
id = "dummy"
manufacturer = "NUT"
model = "overridden"
driver = "dummy-ups"
"#,
        );
        let catalog = load_catalog(&host).unwrap();
        assert_eq!(
            catalog
                .ups(&ProfileId::new("dummy").unwrap())
                .unwrap()
                .model,
            "overridden"
        );
    }
}
