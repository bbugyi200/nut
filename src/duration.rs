use std::time::Duration;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct DurationValue(pub Duration);

impl DurationValue {
    pub fn parse(input: &str) -> Result<Self, DurationError> {
        let trimmed = input.trim();
        let (digits, unit) = trimmed.split_at(
            trimmed
                .find(|character: char| !character.is_ascii_digit())
                .unwrap_or(trimmed.len()),
        );
        if digits.is_empty() || unit.is_empty() {
            return Err(DurationError::Invalid(input.to_owned()));
        }
        let value = digits
            .parse::<u64>()
            .map_err(|_| DurationError::Invalid(input.to_owned()))?;
        let multiplier = match unit {
            "s" => 1,
            "m" => 60,
            "h" => 60 * 60,
            _ => return Err(DurationError::Invalid(input.to_owned())),
        };
        value
            .checked_mul(multiplier)
            .map(Duration::from_secs)
            .map(Self)
            .ok_or_else(|| DurationError::Invalid(input.to_owned()))
    }

    pub fn as_duration(self) -> Duration {
        self.0
    }

    pub fn as_secs(self) -> u64 {
        self.0.as_secs()
    }

    pub fn compact(self) -> String {
        let seconds = self.0.as_secs();
        if seconds != 0 && seconds.is_multiple_of(3600) {
            format!("{}h", seconds / 3600)
        } else if seconds != 0 && seconds.is_multiple_of(60) {
            format!("{}m", seconds / 60)
        } else {
            format!("{seconds}s")
        }
    }
}

impl std::fmt::Display for DurationValue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let seconds = self.0.as_secs();
        if seconds >= 3600 {
            write!(
                formatter,
                "{}h {:02}m",
                seconds / 3600,
                (seconds % 3600) / 60
            )
        } else if seconds >= 60 && !seconds.is_multiple_of(60) {
            write!(formatter, "{}m {:02}s", seconds / 60, seconds % 60)
        } else if seconds >= 60 {
            write!(formatter, "{}m", seconds / 60)
        } else {
            write!(formatter, "{seconds}s")
        }
    }
}

impl Serialize for DurationValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.compact())
    }
}

impl<'de> Deserialize<'de> for DurationValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum DurationError {
    #[error("invalid duration {0:?}; use an integer followed by s, m, or h")]
    Invalid(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_values_parse_and_serialize_as_short_units() {
        assert_eq!(
            DurationValue::parse("25m").unwrap().as_duration(),
            Duration::from_secs(1500)
        );
        assert_eq!(DurationValue::parse("4s").unwrap().to_string(), "4s");
        assert_eq!(DurationValue::parse("180s").unwrap().compact(), "3m");
        assert_eq!(DurationValue::parse("45m").unwrap().compact(), "45m");
        assert!(DurationValue::parse("2d").is_err());
    }
}
