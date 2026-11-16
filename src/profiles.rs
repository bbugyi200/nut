use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
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
}

impl fmt::Display for UsbId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:04x}:{:04x}", self.vendor, self.product)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpsProfile {
    pub id: ProfileId,
    pub manufacturer: String,
    pub model: String,
    #[serde(default)]
    pub model_aliases: Vec<String>,
    pub driver: String,
    #[serde(default)]
    pub usb_ids: Vec<UsbId>,
    pub nominal_va: Option<u32>,
    pub nominal_watts: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoardProfile {
    pub id: ProfileId,
    pub dmi_vendor: String,
    #[serde(default)]
    pub dmi_names: Vec<String>,
    pub bios_vendor: Option<String>,
}

impl BoardProfile {
    pub fn matches(&self, vendor: &str, board_name: &str) -> bool {
        self.dmi_vendor.eq_ignore_ascii_case(vendor)
            && self
                .dmi_names
                .iter()
                .any(|name| name.eq_ignore_ascii_case(board_name))
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

    pub fn insert_board(&mut self, profile: BoardProfile) -> Result<(), ProfileError> {
        if self.boards.contains_key(&profile.id) {
            return Err(ProfileError::Duplicate(profile.id));
        }
        self.boards.insert(profile.id.clone(), profile);
        Ok(())
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let profile = BoardProfile {
            id: ProfileId::new("asus-trx40").unwrap(),
            dmi_vendor: "ASUSTeK COMPUTER INC.".into(),
            dmi_names: vec!["PRIME TRX40-PRO".into()],
            bios_vendor: Some("American Megatrends".into()),
        };
        assert!(profile.matches("asustek computer inc.", "prime trx40-pro"));
        assert!(!profile.matches("ASUSTeK COMPUTER INC.", "System Product Name"));
    }
}
